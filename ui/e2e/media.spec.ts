import type { Media } from "../src/api/generated/Media";
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
const MEDIA_LIST_PATH = new RegExp(`/projects/${PROJECT}/media$`);
const MEDIA_ONE_PATH = new RegExp(`/projects/${PROJECT}/media/[0-9a-f-]{36}$`);
const PAYLOAD_PATH = new RegExp(`/projects/${PROJECT}/spans/${TRACE}/[0-9a-f]{16}/payload$`);
const OBJECTS = "http://objects.e2e:7070";
const IMAGE_ID = "0193a1b2-0000-7000-8000-0000000000e1";
const PDF_ID = "0193a1b2-0000-7000-8000-0000000000e2";

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

function uses(list: Media[]) {
  return json({ data: list.map((m) => ({ span_id: SPAN, field: "input", media: m })) });
}

// The page is signed in with media on, and the span's input is a small JSON document.
function setUp(api: import("./fixtures").Api, enabled = true) {
  const session = sessionInfo();
  session.features.media = { enabled, max_bytes: enabled ? 67_108_864 : 0 };
  signedIn(api, session);
  api.on(TRACE_PATH, json(traceDetail()));
  api.on(PAYLOAD_PATH, { status: 200, contentType: "text/plain", body: '{"q": 1}' });
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

const inputTab = traceUrl({ span: SPAN, tab: "input" });

test.describe("trace detail: media in a payload", () => {
  test.beforeEach(async ({ page }) => {
    await page.route(`${OBJECTS}/**`, (route) =>
      route.fulfill({ status: 200, contentType: "image/png", body: PNG }),
    );
  });

  test("loading: the media of the field hold a skeleton after 300 ms", async ({ page, api }) => {
    setUp(api);
    const release = api.hold(MEDIA_LIST_PATH, uses([IMAGE]));
    await page.goto(inputTab);
    await expect(
      page.getByRole("status").filter({ hasText: "Loading the media of the input" }),
    ).toBeVisible();
    await expectAccessible(page, "media loading");
    release();
    await expect(page.getByRole("img", { name: /Image from the payload/ })).toBeVisible();
  });

  test("success: an image shows inline and decodes, and a document is a download", async ({
    page,
    api,
  }) => {
    setUp(api);
    api.on(MEDIA_LIST_PATH, uses([IMAGE, PDF]));
    await page.goto(inputTab);
    const region = page.getByRole("region", { name: "Media in the input" });
    const image = region.getByRole("img", { name: /Image from the payload, image\/png/ });
    await expect(image).toBeVisible();
    await expect
      .poll(() => image.evaluate((element) => (element as HTMLImageElement).naturalWidth))
      .toBeGreaterThan(0);
    const download = region.getByRole("link", { name: "Download application/pdf, 2 KB" });
    await expect(download).toHaveAttribute(
      "href",
      `${OBJECTS}/media/${PDF_ID}?X-Amz-Signature=e2e`,
    );
    await expectAccessible(page, "media success");
  });

  test("error: a failed list offers a retry, and an image that fails to load offers one too", async ({
    page,
    api,
  }) => {
    setUp(api);
    api.on(MEDIA_LIST_PATH, problem(503, "control_plane_unavailable", "req-media"));
    await page.goto(inputTab);
    const retry = page.getByRole("button", { name: "Retry media of the input" });
    await expect(retry).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText("req-media")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "media error");

    // The object store refuses the link: the image says so and reads a fresh one on retry.
    await page.route(`${OBJECTS}/**`, (route) => route.fulfill({ status: 403, body: "" }));
    api.on(MEDIA_LIST_PATH, uses([IMAGE]));
    await retry.click();
    await expect(page.getByText("This media couldn't be shown.")).toBeVisible();
    await page.route(`${OBJECTS}/**`, (route) =>
      route.fulfill({ status: 200, contentType: "image/png", body: PNG }),
    );
    await page.getByRole("button", { name: "Retry media" }).click();
    await expect(page.getByRole("img", { name: /Image from the payload/ })).toBeVisible();
  });

  test("empty: no media add nothing, and with media off nothing is asked for", async ({
    page,
    api,
  }) => {
    setUp(api);
    api.on(MEDIA_LIST_PATH, uses([]));
    await page.goto(inputTab);
    await expect(page.getByRole("tree", { name: "Input" })).toBeVisible();
    await expect.poll(() => api.count(MEDIA_LIST_PATH)).toBe(1);
    await expect(page.getByRole("region", { name: "Media in the input" })).toHaveCount(0);
    await expectAccessible(page, "media empty");
  });

  test("empty: a deployment without media never asks for them", async ({ page, api }) => {
    setUp(api, false);
    await page.goto(inputTab);
    await expect(page.getByRole("tree", { name: "Input" })).toBeVisible();
    expect(api.count(MEDIA_LIST_PATH)).toBe(0);
  });
});
