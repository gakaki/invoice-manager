-- 发票管理 D1 schema (initial migration)
-- Design notes for the Cloudflare D1 daily row-read budget:
--   * Every list/search query uses an indexed WHERE + explicit columns + LIMIT/offset pagination.
--   * No `SELECT *`, no unbounded full-table scans, no per-row N+1 loops.
--   * Amounts are stored in cents (INTEGER) to avoid float drift and keep aggregation cheap.

CREATE TABLE IF NOT EXISTS invoices (
  id            TEXT    PRIMARY KEY,           -- client-generated UUID
  invoice_code  TEXT    NOT NULL DEFAULT '',   -- 发票代码
  invoice_number TEXT   NOT NULL DEFAULT '',   -- 发票号码
  invoice_date  TEXT    NOT NULL DEFAULT '',   -- 开票日期 (YYYY-MM-DD)
  buyer_name    TEXT    NOT NULL DEFAULT '',   -- 购买方名称
  buyer_taxno   TEXT    NOT NULL DEFAULT '',   -- 购买方税号
  seller_name   TEXT    NOT NULL DEFAULT '',   -- 销售方名称
  seller_taxno  TEXT    NOT NULL DEFAULT '',   -- 销售方税号
  amount_cents  INTEGER NOT NULL DEFAULT 0,    -- 金额 (不含税), 以分为单位
  tax_cents     INTEGER NOT NULL DEFAULT 0,    -- 税额, 以分为单位
  total_cents   INTEGER NOT NULL DEFAULT 0,    -- 价税合计, 以分为单位
  raw_text      TEXT    NOT NULL DEFAULT '',   -- OCR/输入的原始文本
  source        TEXT    NOT NULL DEFAULT 'manual', -- manual | ocr | text
  created_at    INTEGER NOT NULL,              -- epoch ms
  updated_at    INTEGER NOT NULL               -- epoch ms
);

-- Indexes that back the only query shapes the app uses (list by recency, search by number/seller).
CREATE INDEX IF NOT EXISTS idx_invoices_created_at   ON invoices (created_at DESC);
CREATE INDEX IF NOT EXISTS idx_invoices_number       ON invoices (invoice_number);
CREATE INDEX IF NOT EXISTS idx_invoices_seller       ON invoices (seller_name);
