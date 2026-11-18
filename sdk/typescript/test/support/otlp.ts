// A minimal decoder of OTLP `ExportTraceServiceRequest` protobuf bodies, for the fake collector. Integers decode
// to `bigint` and doubles to `number`, so tests can tell the two apart.

export type Value =
  | string
  | boolean
  | bigint
  | number
  | Uint8Array
  | Value[]
  | { [key: string]: Value };

export interface DecodedSpan {
  traceId: string;
  spanId: string;
  parentSpanId: string;
  traceState: string;
  name: string;
  kind: number;
  startTimeUnixNano: bigint;
  endTimeUnixNano: bigint;
  attributes: Record<string, Value>;
  events: { name: string; attributes: Record<string, Value> }[];
  status: { code: number; message: string };
  resource: Record<string, Value>;
  scope: { name: string; version: string };
}

class Reader {
  readonly #bytes: Uint8Array;
  #pos: number;
  readonly #end: number;

  constructor(bytes: Uint8Array, start = 0, end = bytes.length) {
    this.#bytes = bytes;
    this.#pos = start;
    this.#end = end;
  }

  done(): boolean {
    return this.#pos >= this.#end;
  }

  varint(): bigint {
    let result = 0n;
    let shift = 0n;
    for (;;) {
      const byte = this.#bytes[this.#pos];
      if (byte === undefined || this.#pos >= this.#end) {
        throw new Error("truncated varint");
      }
      this.#pos += 1;
      result |= BigInt(byte & 0x7f) << shift;
      if ((byte & 0x80) === 0) {
        return result;
      }
      shift += 7n;
    }
  }

  fixed64(): DataView {
    const view = new DataView(this.#bytes.buffer, this.#bytes.byteOffset + this.#pos, 8);
    this.#pos += 8;
    return view;
  }

  /** A sub-reader over the next length-delimited field. */
  sub(): Reader {
    const length = Number(this.varint());
    const reader = new Reader(this.#bytes, this.#pos, this.#pos + length);
    this.#pos += length;
    return reader;
  }

  bytes(): Uint8Array {
    const length = Number(this.varint());
    const slice = this.#bytes.subarray(this.#pos, this.#pos + length);
    this.#pos += length;
    return slice;
  }

  string(): string {
    return new TextDecoder().decode(this.bytes());
  }

  skip(wireType: number): void {
    if (wireType === 0) {
      this.varint();
    } else if (wireType === 1) {
      this.#pos += 8;
    } else if (wireType === 2) {
      this.bytes();
    } else if (wireType === 5) {
      this.#pos += 4;
    } else {
      throw new Error(`unsupported wire type ${wireType}`);
    }
  }

  /** Calls `on(field, wireType)` for each field; `on` returns false when it did not consume the field. */
  fields(on: (field: number, wireType: number) => boolean): void {
    while (!this.done()) {
      const tag = Number(this.varint());
      const field = tag >>> 3;
      const wireType = tag & 7;
      if (!on(field, wireType)) {
        this.skip(wireType);
      }
    }
  }
}

function hex(bytes: Uint8Array): string {
  return Buffer.from(bytes).toString("hex");
}

function anyValue(reader: Reader): Value {
  let value: Value = "";
  reader.fields((field) => {
    if (field === 1) {
      value = reader.string();
    } else if (field === 2) {
      value = reader.varint() !== 0n;
    } else if (field === 3) {
      value = BigInt.asIntN(64, reader.varint());
    } else if (field === 4) {
      value = reader.fixed64().getFloat64(0, true);
    } else if (field === 5) {
      const items: Value[] = [];
      const array = reader.sub();
      array.fields((inner) => {
        if (inner !== 1) {
          return false;
        }
        items.push(anyValue(array.sub()));
        return true;
      });
      value = items;
    } else if (field === 6) {
      const object: Record<string, Value> = {};
      const list = reader.sub();
      list.fields((inner) => {
        if (inner !== 1) {
          return false;
        }
        const [key, member] = keyValue(list.sub());
        object[key] = member;
        return true;
      });
      value = object;
    } else if (field === 7) {
      value = reader.bytes();
    } else {
      return false;
    }
    return true;
  });
  return value;
}

function keyValue(reader: Reader): [string, Value] {
  let key = "";
  let value: Value = "";
  reader.fields((field) => {
    if (field === 1) {
      key = reader.string();
    } else if (field === 2) {
      value = anyValue(reader.sub());
    } else {
      return false;
    }
    return true;
  });
  return [key, value];
}

function attributesOf(reader: Reader, into: Record<string, Value>): void {
  const [key, value] = keyValue(reader);
  into[key] = value;
}

function span(
  reader: Reader,
  resource: Record<string, Value>,
  scope: DecodedSpan["scope"],
): DecodedSpan {
  const decoded: DecodedSpan = {
    traceId: "",
    spanId: "",
    parentSpanId: "",
    traceState: "",
    name: "",
    kind: 0,
    startTimeUnixNano: 0n,
    endTimeUnixNano: 0n,
    attributes: {},
    events: [],
    status: { code: 0, message: "" },
    resource,
    scope,
  };
  reader.fields((field) => {
    switch (field) {
      case 1:
        decoded.traceId = hex(reader.bytes());
        return true;
      case 2:
        decoded.spanId = hex(reader.bytes());
        return true;
      case 3:
        decoded.traceState = reader.string();
        return true;
      case 4:
        decoded.parentSpanId = hex(reader.bytes());
        return true;
      case 5:
        decoded.name = reader.string();
        return true;
      case 6:
        decoded.kind = Number(reader.varint());
        return true;
      case 7:
        decoded.startTimeUnixNano = reader.fixed64().getBigUint64(0, true);
        return true;
      case 8:
        decoded.endTimeUnixNano = reader.fixed64().getBigUint64(0, true);
        return true;
      case 9:
        attributesOf(reader.sub(), decoded.attributes);
        return true;
      case 11: {
        const event = reader.sub();
        const parsed = { name: "", attributes: {} as Record<string, Value> };
        event.fields((inner) => {
          if (inner === 2) {
            parsed.name = event.string();
          } else if (inner === 3) {
            attributesOf(event.sub(), parsed.attributes);
          } else {
            return false;
          }
          return true;
        });
        decoded.events.push(parsed);
        return true;
      }
      case 15: {
        const status = reader.sub();
        status.fields((inner) => {
          if (inner === 2) {
            decoded.status.message = status.string();
          } else if (inner === 3) {
            decoded.status.code = Number(status.varint());
          } else {
            return false;
          }
          return true;
        });
        return true;
      }
      default:
        return false;
    }
  });
  return decoded;
}

/** Decodes an `ExportTraceServiceRequest` into its spans, each with its resource and scope. */
export function decodeTraces(body: Uint8Array): DecodedSpan[] {
  const spans: DecodedSpan[] = [];
  const top = new Reader(body);
  top.fields((field) => {
    if (field !== 1) {
      return false;
    }
    const rs = top.sub();
    const resource: Record<string, Value> = {};
    const scopes: Reader[] = [];
    rs.fields((inner) => {
      if (inner === 1) {
        const res = rs.sub();
        res.fields((f) => {
          if (f !== 1) {
            return false;
          }
          attributesOf(res.sub(), resource);
          return true;
        });
        return true;
      }
      if (inner === 2) {
        scopes.push(rs.sub());
        return true;
      }
      return false;
    });
    for (const ss of scopes) {
      const scope = { name: "", version: "" };
      const raw: Reader[] = [];
      ss.fields((inner) => {
        if (inner === 1) {
          const sc = ss.sub();
          sc.fields((f) => {
            if (f === 1) {
              scope.name = sc.string();
            } else if (f === 2) {
              scope.version = sc.string();
            } else {
              return false;
            }
            return true;
          });
          return true;
        }
        if (inner === 2) {
          raw.push(ss.sub());
          return true;
        }
        return false;
      });
      for (const s of raw) {
        spans.push(span(s, resource, scope));
      }
    }
    return true;
  });
  return spans;
}
