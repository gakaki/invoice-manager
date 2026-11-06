// Real UI screenshots for the README. Run on a real platform so the window is
// actually drawn:
//   nsh test Screenshots.test.tsx --target macos
// A headless run draws nothing, so `capture` no-ops there (CI `nsh test` stays
// green). Outputs are PNGs; convert to WebP before committing.
import { expect, open, test } from "nsh/test";

// Realistic Chinese invoice records, matching the Worker's /api/invoices shape.
const INVOICES = [
  {
    id: "11111111-1111-4111-8111-111111111111",
    invoice_code: "011002200311",
    invoice_number: "24312000000012345678",
    invoice_date: "2026-10-28",
    buyer_name: "上海知微软件有限公司",
    buyer_taxno: "91310115MA1K3X2Y8Z",
    seller_name: "北京云票信息技术有限公司",
    seller_taxno: "91110108MA01C8XY2K",
    amount_cents: 128000,
    tax_cents: 16640,
    total_cents: 144640,
    raw_text: "",
    source: "ocr",
    created_at: 1761609600000,
    updated_at: 1761609600000,
  },
  {
    id: "22222222-2222-4222-8222-222222222222",
    invoice_code: "011002200311",
    invoice_number: "24312000000087654321",
    invoice_date: "2026-10-25",
    buyer_name: "上海知微软件有限公司",
    buyer_taxno: "91310115MA1K3X2Y8Z",
    seller_name: "杭州迅捷办公设备有限公司",
    seller_taxno: "91330106MA2B1N5P4R",
    amount_cents: 368000,
    tax_cents: 47840,
    total_cents: 415840,
    raw_text: "",
    source: "ocr",
    created_at: 1761350400000,
    updated_at: 1761350400000,
  },
  {
    id: "33333333-3333-4333-8333-333333333333",
    invoice_code: "011002200311",
    invoice_number: "24312000000011223344",
    invoice_date: "2026-10-21",
    buyer_name: "上海知微软件有限公司",
    buyer_taxno: "91310115MA1K3X2Y8Z",
    seller_name: "广州差旅服务有限公司",
    seller_taxno: "91440101MA5CT8K21D",
    amount_cents: 58600,
    tax_cents: 3516,
    total_cents: 62116,
    raw_text: "",
    source: "text",
    created_at: 1761004800000,
    updated_at: 1761004800000,
  },
];

// The launch-time list request and every other call return our fixture data.
const network = { "*": { body: { invoices: INVOICES } } };
const options = { network, width: 440, height: 820 };
const OUT = "../assets/screenshots";

/** No-op on a headless engine (it draws nothing). */
async function capture(app: any, name: string): Promise<void> {
  let png: Uint8Array;
  try {
    png = await app.screenshot();
  } catch {
    return;
  }
  if (png.length < 1000) return;
  await app.screenshot(`${OUT}/${name}.png`);
  expect(png.length).toBeGreaterThan(1000);
}

test("01 发票列表（主界面）", async () => {
  const app = await open(".", options);
  await app.wait(400);
  await capture(app, "01-list");
});

test("02 录入发票（文字解析 / 图片 OCR）", async () => {
  const app = await open(".", options);
  await app.wait(400);
  await app.press("添加");
  await app.wait(300);
  // Paste a real invoice text atomically, then parse it so the fields fill in.
  // (Typing key-by-key re-renders the controlled input and loses the caret.)
  await app.paste("text:粘贴或输入发票文字，然后点「解析字段」", {
    text: "发票代码：011002200311\n发票号码：24312000000099887766\n开票日期：2026-11-01\n购买方名称：上海知微软件有限公司\n购买方税号：91310115MA1K3X2Y8Z\n销售方名称：深圳云创科技有限公司\n销售方税号：91440300MA5D8K2L1P\n金额：860.00\n税额：111.80\n价税合计：971.80",
  });
  await app.press("解析字段");
  await app.wait(300);
  await capture(app, "02-add");
});

test("03 多选拼接输出", async () => {
  const app = await open(".", options);
  await app.wait(400);
  await app.press("id:select-24312000000012345678");
  await app.press("id:select-24312000000087654321");
  await app.wait(200);
  await app.press("输出选中");
  await app.wait(300);
  await capture(app, "03-output");
});

test("04 设置（后端地址 / Cloudflare D1）", async () => {
  const app = await open(".", options);
  await app.wait(400);
  await app.press("设置");
  await app.wait(300);
  await capture(app, "04-settings");
});
