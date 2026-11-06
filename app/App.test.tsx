// 发票管理 — headless smoke tests for the Rust root (`nsh.json` "root": "App").
// `open(".")` mounts the app on a headless engine; the network is stubbed so
// the launch-time D1 list request never leaves the machine.
import { expect, open, test } from "nsh/test";

// The Worker's GET /api/invoices response shape: an empty list.
const EMPTY_LIST = { body: { invoices: [] } };
const network = {
  // Exact launch request, plus a wildcard in case api_base/query differs.
  "GET https://invoice.gakaki.com/api/invoices?limit=200": EMPTY_LIST,
  "*": EMPTY_LIST,
};

test("启动后进入发票列表，并展示录入与设置入口", async () => {
  const app = await open(".", { network });
  const texts = app.texts();
  expect(texts).toContain("发票管理");
  expect(texts).toContain("刷新");
  expect(texts).toContain("设置");
  expect(texts).toContain("添加");
});

test("列表为空时显示引导文案", async () => {
  const app = await open(".", { network });
  expect(app.texts().join("\n")).toContain("还没有发票");
});
