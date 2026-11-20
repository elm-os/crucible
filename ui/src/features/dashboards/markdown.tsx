import type { ReactNode } from "react";

// A text widget's Markdown: headings, bullet lists, paragraphs, **bold**, *italic*, `code` and links. Text is
// never parsed as HTML, and a link opens only an http or https address; anything else stays plain text.

type Block =
  | { kind: "heading"; level: 1 | 2 | 3; text: string }
  | { kind: "list"; items: string[] }
  | { kind: "paragraph"; text: string };

export function blocks(source: string): Block[] {
  const out: Block[] = [];
  let paragraph: string[] = [];
  let list: string[] | null = null;
  const flush = () => {
    if (paragraph.length > 0) out.push({ kind: "paragraph", text: paragraph.join(" ") });
    if (list) out.push({ kind: "list", items: list });
    paragraph = [];
    list = null;
  };
  for (const raw of source.split(/\r?\n/)) {
    const line = raw.trim();
    const heading = /^(#{1,3})\s+(.*)$/.exec(line);
    const item = /^[-*]\s+(.*)$/.exec(line);
    if (line === "") {
      flush();
    } else if (heading) {
      flush();
      out.push({
        kind: "heading",
        level: (heading[1]?.length ?? 1) as 1 | 2 | 3,
        text: heading[2] ?? "",
      });
    } else if (item) {
      if (paragraph.length > 0) flush();
      list ??= [];
      list.push(item[1] ?? "");
    } else {
      if (list) flush();
      paragraph.push(line);
    }
  }
  flush();
  return out;
}

const INLINE = /`([^`]+)`|\*\*([^*]+)\*\*|\*([^*]+)\*|_([^_]+)_|\[([^\]]+)\]\(([^)\s]+)\)/g;

export function safeHref(href: string): string | null {
  try {
    const url = new URL(href);
    return url.protocol === "http:" || url.protocol === "https:" ? url.href : null;
  } catch {
    return null;
  }
}

export function inline(text: string): ReactNode[] {
  const nodes: ReactNode[] = [];
  let last = 0;
  for (const match of text.matchAll(INLINE)) {
    const at = match.index ?? 0;
    if (at > last) nodes.push(text.slice(last, at));
    const [whole, code, bold, star, underscore, label, href] = match;
    const key = `${at}`;
    if (code !== undefined) nodes.push(<code key={key}>{code}</code>);
    else if (bold !== undefined) nodes.push(<strong key={key}>{bold}</strong>);
    else if (star !== undefined || underscore !== undefined) {
      nodes.push(<em key={key}>{star ?? underscore}</em>);
    } else if (label !== undefined && href !== undefined) {
      const safe = safeHref(href);
      nodes.push(
        safe ? (
          <a key={key} href={safe} target="_blank" rel="noreferrer noopener">
            {label}
          </a>
        ) : (
          whole
        ),
      );
    }
    last = at + whole.length;
  }
  if (last < text.length) nodes.push(text.slice(last));
  return nodes;
}

const HEADING_CLASS = { 1: "text-body font-medium", 2: "text-data font-medium", 3: "text-data" };

export function Markdown({ source }: { source: string }) {
  return (
    <div className="flex flex-col gap-2 text-data">
      {blocks(source).map((block, i) => {
        const key = `${block.kind}-${i}`;
        if (block.kind === "heading") {
          // The widget's own title is the section's heading, so the text's headings sit below it.
          const Tag = block.level === 1 ? "h3" : block.level === 2 ? "h4" : "h5";
          return (
            <Tag key={key} className={HEADING_CLASS[block.level]}>
              {inline(block.text)}
            </Tag>
          );
        }
        if (block.kind === "list") {
          return (
            <ul key={key} className="list-disc ps-5">
              {block.items.map((item, j) => (
                <li key={`${j}-${item}`}>{inline(item)}</li>
              ))}
            </ul>
          );
        }
        return <p key={key}>{inline(block.text)}</p>;
      })}
    </div>
  );
}
