import type { Media } from "../src/api/generated/Media";
import type { SpanMedia } from "../src/api/generated/SpanMedia";
import {
  expect,
  expectAccessible,
  json,
  PROJECT,
  problem,
  SERVER_TEXT,
  sessionInfo,
  signedIn,
  TRACE,
  TRACE_PATH,
  test,
  traceDetail,
  traceUrl,
} from "./fixtures";

const SPAN = "a000000000000002";
const SPAN_MEDIA_PATH = new RegExp(`/projects/${PROJECT}/spans/${TRACE}/${SPAN}/media$`);
const MEDIA_ONE_PATH = new RegExp(`/projects/${PROJECT}/media/[0-9a-f-]{36}$`);
const OBJECTS = "http://objects.e2e:7070";
const IMAGE_ID = "0193a1b2-0000-7000-8000-0000000000e1";
const PDF_ID = "0193a1b2-0000-7000-8000-0000000000e2";
const GUIDE = "https://github.com/elm-os/crucible/blob/main/docs/self-hosting/media.md";

// A 1x1 PNG, so that the page decodes a real image.
const PNG = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==",
  "base64",
);

function media(id: string, fields: Partial<Media> = {}): Media {
  return {
    id,
    content_type: "image/png",
    len: PNG.length,
    sha256: "ab".repeat(32),
    state: "uploaded",
    disposition: "inline",
    created_at: "2026-11-17T14:00:00.000000Z",
    uploaded_at: "2026-11-17T14:00:01.000000Z",
    saturated: false,
    ...fields,
  };
}

const IMAGE = media(IMAGE_ID);
const PDF = media(PDF_ID, {
  content_type: "application/pdf",
  len: 2_048,
  disposition: "attachment",
});

const REFERENCES: SpanMedia[] = [
  {
    field: "input",
    pointer: "/0/content/1/image_url/url",
    content_type: "image/png",
    media: IMAGE,
  },
  { field: "input", pointer: "/0/content/2", content_type: "audio/wav" },
  { field: "metadata", pointer: "/report", content_type: "application/pdf", media: PDF },
];

const references = (data: SpanMedia[]) => json({ data });

// The page is signed in, with media on unless `enabled` is false.
function setUp(api: import("./fixtures").Api, enabled = true) {
  const session = sessionInfo();
  session.features.media = { enabled, max_bytes: enabled ? 67_108_864 : 0 };
  signedIn(api, session);
  api.on(TRACE_PATH, json(traceDetail()));
  api.on(MEDIA_ONE_PATH, (route, url) => {
    const id = url.pathname.split("/").pop();
    const found = [IMAGE, PDF].find((m) => m.id === id);
    const link = `${OBJECTS}/media/${id}?X-Amz-Signature=e2e`;
    return route.fulfill(
      found
        ? json({ ...found, url: link, url_expires_at: "2026-11-17T14:05:00.000000Z" })
        : problem(404, "not_found"),
    );
  });
}

const mediaTab = traceUrl({ span: SPAN, tab: "media" });

test.describe("trace detail: the media of a span", () => {
  test.beforeEach(async ({ page }) => {
    await page.route(`${OBJECTS}/**`, (route) =>
      route.fulfill({ status: 200, contentType: "image/png", body: PNG }),
    );
  });

  test("loading: the span's media hold a skeleton after 300 ms", async ({ page, api }) => {
    setUp(api);
    const release = api.hold(SPAN_MEDIA_PATH, references([REFERENCES[0]!]));
    await page.goto(mediaTab);
    await expect(
      page.getByRole("status").filter({ hasText: "Loading the media of this span" }),
    ).toBeVisible();
    await expectAccessible(page, "span media loading");
    release();
    await expect(page.getByRole("img", { name: /Image from the payload/ })).toBeVisible();
  });

  test("success: an image shows inline and decodes, a document is a download, and a missing upload says so", async ({
    page,
    api,
  }) => {
    setUp(api);
    api.on(SPAN_MEDIA_PATH, references(REFERENCES));
    await page.goto(mediaTab);
    const list = page.getByRole("list", { name: "Media of this span" });
    await expect(list.getByText("In the input at /0/content/1/image_url/url")).toBeVisible();
    const image = list.getByRole("img", { name: /Image from the payload, image\/png/ });
    await expect(image).toBeVisible();
    await expect
      .poll(() => image.evaluate((element) => (element as HTMLImageElement).naturalWidth))
      .toBeGreaterThan(0);
    await expect(list.getByText("Not stored")).toBeVisible();
    await expect(list.getByText("In the metadata at /report")).toBeVisible();
    const download = list.getByRole("link", { name: "Download application/pdf, 2 KB" });
    await expect(download).toHaveAttribute(
      "href",
      `${OBJECTS}/media/${PDF_ID}?X-Amz-Signature=e2e`,
    );
    await expectAccessible(page, "span media success");
  });

  test("error: a failed list offers a retry, and an image that fails to load offers one too", async ({
    page,
    api,
  }) => {
    setUp(api);
    api.on(SPAN_MEDIA_PATH, problem(503, "telemetry_store_unavailable", "req-media"));
    await page.goto(mediaTab);
    const retry = page.getByRole("button", { name: "Retry media of this span" });
    await expect(retry).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText("req-media")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "span media error");

    // The object store refuses the link: the image says so and reads a fresh one on retry.
    await page.route(`${OBJECTS}/**`, (route) => route.fulfill({ status: 403, body: "" }));
    api.on(SPAN_MEDIA_PATH, references([REFERENCES[0]!]));
    await retry.click();
    await expect(page.getByText("This media couldn't be shown.")).toBeVisible();
    await page.route(`${OBJECTS}/**`, (route) =>
      route.fulfill({ status: 200, contentType: "image/png", body: PNG }),
    );
    await page.getByRole("button", { name: "Retry media" }).click();
    await expect(page.getByRole("img", { name: /Image from the payload/ })).toBeVisible();
  });

  test("empty: a span without media says so, and one whose payload expired says that", async ({
    page,
    api,
  }) => {
    setUp(api);
    api.on(SPAN_MEDIA_PATH, references([]));
    await page.goto(mediaTab);
    await expect(page.getByText("This span references no media.")).toBeVisible();
    await expectAccessible(page, "span media empty");

    api.on(SPAN_MEDIA_PATH, problem(404, "not_found"));
    await page.reload();
    await expect(page.getByText("This span's payload isn't stored.")).toBeVisible();
    await expect(page.getByRole("button", { name: /Retry/ })).toHaveCount(0);
  });

  test("no object storage: one sentence and the guide, and nothing is asked for", async ({
    page,
    api,
  }) => {
    setUp(api, false);
    await page.goto(mediaTab);
    await expect(
      page.getByText("This server has no object storage, so it keeps no media."),
    ).toBeVisible();
    await expect(page.getByRole("link", { name: "How to turn media on" })).toHaveAttribute(
      "href",
      GUIDE,
    );
    expect(api.count(SPAN_MEDIA_PATH)).toBe(0);
    await expectAccessible(page, "span media without object storage");
  });
});
