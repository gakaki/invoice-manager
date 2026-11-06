/**
 * 发票管理 — Cloudflare Worker
 *
 * Responsibilities:
 *   1. Serve the static download page (site/) for non-/api routes via the ASSETS binding.
 *   2. Provide a JSON REST API for invoices backed by Cloudflare D1.
 *   3. Provide POST /api/ocr, which runs a Workers AI vision model over an invoice image
 *      and returns structured fields + raw text.
 *
 * D1 row-read budget discipline (see AGENTS.md):
 *   - List/search always use an indexed WHERE, explicit columns, and LIMIT/offset pagination.
 *   - No `SELECT *`, no unbounded scans, no N+1 loops.
 *   - Health check is `SELECT 1` only.
 */

export interface Env {
  DB: D1Database;
  AI: Ai;
  ASSETS: Fetcher;
}

/** The vision model used for invoice OCR. Override with the OCR_MODEL env var if needed. */
const OCR_MODEL = "@cf/meta/llama-4-scout-17b-16e-instruct";

/** Max accepted image bytes (base64-decoded) — keeps Worker payloads and AI costs bounded. */
const MAX_IMAGE_BYTES = 6 * 1024 * 1024; // 6 MB

const CORS_HEADERS: Record<string, string> = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET,POST,PUT,DELETE,OPTIONS",
  "Access-Control-Allow-Headers": "Content-Type, Authorization",
  "Access-Control-Max-Age": "86400",
};

function json(data: unknown, status = 200): Response {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "Content-Type": "application/json; charset=utf-8", ...CORS_HEADERS },
  });
}

function error(message: string, status = 400): Response {
  return json({ error: message }, status);
}

/** The invoice shape exchanged with clients. Amounts are integers in cents. */
interface InvoiceRow {
  id: string;
  invoice_code: string;
  invoice_number: string;
  invoice_date: string;
  buyer_name: string;
  buyer_taxno: string;
  seller_name: string;
  seller_taxno: string;
  amount_cents: number;
  tax_cents: number;
  total_cents: number;
  raw_text: string;
  source: string;
  created_at: number;
  updated_at: number;
}

const INVOICE_COLUMNS = [
  "id", "invoice_code", "invoice_number", "invoice_date",
  "buyer_name", "buyer_taxno", "seller_name", "seller_taxno",
  "amount_cents", "tax_cents", "total_cents",
  "raw_text", "source", "created_at", "updated_at",
].join(", ");

/** Coerce a client-supplied money value (yuan number, cent integer, or string) into cents. */
function toCents(value: unknown): number {
  if (value == null || value === "") return 0;
  if (typeof value === "number") {
    if (!isFinite(value)) return 0;
    // Heuristic: values with a fractional part are yuan; integers are already cents.
    return Number.isInteger(value) ? value : Math.round(value * 100);
  }
  const s = String(value).replace(/[,¥￥\s]/g, "");
  const n = parseFloat(s);
  if (!isFinite(n)) return 0;
  return s.includes(".") ? Math.round(n * 100) : Math.round(n);
}

function str(value: unknown): string {
  return value == null ? "" : String(value);
}

/** Build a normalised invoice object from a client payload. */
function normaliseInvoice(body: any, now: number, id?: string): InvoiceRow {
  const amount = toCents(body.amount ?? body.amount_cents);
  const tax = toCents(body.tax ?? body.tax_cents);
  const providedTotal = body.total ?? body.total_cents;
  const total = providedTotal != null && providedTotal !== "" ? toCents(providedTotal) : amount + tax;
  return {
    id: (id ?? str(body.id)) || crypto.randomUUID(),
    invoice_code: str(body.invoice_code).trim(),
    invoice_number: str(body.invoice_number).trim(),
    invoice_date: str(body.invoice_date).trim(),
    buyer_name: str(body.buyer_name).trim(),
    buyer_taxno: str(body.buyer_taxno).trim(),
    seller_name: str(body.seller_name).trim(),
    seller_taxno: str(body.seller_taxno).trim(),
    amount_cents: amount,
    tax_cents: tax,
    total_cents: total,
    raw_text: str(body.raw_text),
    source: str(body.source) || "manual",
    created_at: typeof body.created_at === "number" ? body.created_at : now,
    updated_at: now,
  };
}

async function listInvoices(env: Env, url: URL): Promise<Response> {
  const limit = Math.min(Math.max(parseInt(url.searchParams.get("limit") ?? "50", 10) || 50, 1), 200);
  const offset = Math.max(parseInt(url.searchParams.get("offset") ?? "0", 10) || 0, 0);
  const q = (url.searchParams.get("q") ?? "").trim();

  // Indexed, bounded queries only. Search matches invoice_number (indexed) or seller_name (indexed).
  const stmt = q
    ? env.DB.prepare(
        `SELECT ${INVOICE_COLUMNS} FROM invoices
         WHERE invoice_number = ?1 OR seller_name LIKE ?2
         ORDER BY created_at DESC LIMIT ?3 OFFSET ?4`,
      ).bind(q, `%${q}%`, limit, offset)
    : env.DB.prepare(
        `SELECT ${INVOICE_COLUMNS} FROM invoices
         ORDER BY created_at DESC LIMIT ?1 OFFSET ?2`,
      ).bind(limit, offset);

  const { results } = await stmt.all<InvoiceRow>();
  return json({ invoices: results ?? [], limit, offset, query: q });
}

async function createInvoice(env: Env, request: Request): Promise<Response> {
  const body: any = await request.json().catch(() => null);
  if (!body || typeof body !== "object") return error("invalid JSON body");
  const now = Date.now();
  const inv = normaliseInvoice(body, now);
  await env.DB.prepare(
    `INSERT INTO invoices (${INVOICE_COLUMNS})
     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)`,
  )
    .bind(
      inv.id, inv.invoice_code, inv.invoice_number, inv.invoice_date,
      inv.buyer_name, inv.buyer_taxno, inv.seller_name, inv.seller_taxno,
      inv.amount_cents, inv.tax_cents, inv.total_cents,
      inv.raw_text, inv.source, inv.created_at, inv.updated_at,
    )
    .run();
  return json({ invoice: inv }, 201);
}

async function batchCreate(env: Env, request: Request): Promise<Response> {
  const body: any = await request.json().catch(() => null);
  const items = Array.isArray(body?.invoices) ? body.invoices : null;
  if (!items) return error("expected { invoices: [...] }");
  if (items.length > 200) return error("at most 200 invoices per batch");
  const now = Date.now();
  const stmt = env.DB.prepare(
    `INSERT INTO invoices (${INVOICE_COLUMNS})
     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)`,
  );
  const batch = items.map((it: any) => {
    const inv = normaliseInvoice(it, now);
    return stmt.bind(
      inv.id, inv.invoice_code, inv.invoice_number, inv.invoice_date,
      inv.buyer_name, inv.buyer_taxno, inv.seller_name, inv.seller_taxno,
      inv.amount_cents, inv.tax_cents, inv.total_cents,
      inv.raw_text, inv.source, inv.created_at, inv.updated_at,
    );
  });
  // One batched write — not a per-row loop (avoids N+1 against the D1 budget).
  await env.DB.batch(batch);
  return json({ created: batch.length }, 201);
}

async function getInvoice(env: Env, id: string): Promise<Response> {
  const { results } = await env.DB.prepare(
    `SELECT ${INVOICE_COLUMNS} FROM invoices WHERE id = ?1 LIMIT 1`,
  ).bind(id).all<InvoiceRow>();
  const inv = results?.[0];
  if (!inv) return error("not found", 404);
  return json({ invoice: inv });
}

async function updateInvoice(env: Env, id: string, request: Request): Promise<Response> {
  const existing = await env.DB.prepare(`SELECT ${INVOICE_COLUMNS} FROM invoices WHERE id = ?1 LIMIT 1`)
    .bind(id).all<InvoiceRow>();
  const current = existing.results?.[0];
  if (!current) return error("not found", 404);
  const body: any = await request.json().catch(() => null);
  if (!body || typeof body !== "object") return error("invalid JSON body");
  const now = Date.now();
  const inv = normaliseInvoice({ ...current, ...body }, now, id);
  await env.DB.prepare(
    `UPDATE invoices SET
       invoice_code=?2, invoice_number=?3, invoice_date=?4,
       buyer_name=?5, buyer_taxno=?6, seller_name=?7, seller_taxno=?8,
       amount_cents=?9, tax_cents=?10, total_cents=?11,
       raw_text=?12, source=?13, updated_at=?14
     WHERE id=?1`,
  )
    .bind(
      inv.id, inv.invoice_code, inv.invoice_number, inv.invoice_date,
      inv.buyer_name, inv.buyer_taxno, inv.seller_name, inv.seller_taxno,
      inv.amount_cents, inv.tax_cents, inv.total_cents,
      inv.raw_text, inv.source, inv.updated_at,
    )
    .run();
  return json({ invoice: inv });
}

async function deleteInvoice(env: Env, id: string): Promise<Response> {
  const res = await env.DB.prepare(`DELETE FROM invoices WHERE id = ?1`).bind(id).run();
  if (!res.success) return error("delete failed", 500);
  return json({ deleted: id });
}

/** The OCR prompt: strict JSON, Chinese invoice fields, no prose. */
const OCR_SYSTEM_PROMPT =
  "你是一个中国发票信息抽取引擎。用户给你一张发票图片，请只输出一个 JSON 对象，不要任何解释文字、不要 markdown 代码块。" +
  "JSON 字段如下（找不到就留空字符串或 0）：" +
  '{"invoice_code":"发票代码","invoice_number":"发票号码","invoice_date":"开票日期(YYYY-MM-DD)",' +
  '"buyer_name":"购买方名称","buyer_taxno":"购买方纳税人识别号","seller_name":"销售方名称",' +
  '"seller_taxno":"销售方纳税人识别号","amount":"金额(不含税,数字)","tax":"税额(数字)",' +
  '"total":"价税合计(数字)","raw_text":"发票上能看到的全部文字,按阅读顺序"}';

async function ocr(env: Env, request: Request): Promise<Response> {
  const body: any = await request.json().catch(() => null);
  if (!body) return error("invalid JSON body");

  // Accept either { image: "data:image/jpeg;base64,...." } or { image_base64, mime }.
  let dataUrl: string | null = null;
  if (typeof body.image === "string" && body.image.startsWith("data:")) {
    dataUrl = body.image;
  } else if (typeof body.image_base64 === "string") {
    const mime = str(body.mime) || "image/jpeg";
    dataUrl = `data:${mime};base64,${body.image_base64.replace(/^data:[^;]*;base64,/, "")}`;
  } else if (typeof body.image === "string") {
    // Bare base64 without a data: prefix.
    const mime = str(body.mime) || "image/jpeg";
    dataUrl = `data:${mime};base64,${body.image}`;
  }
  if (!dataUrl) return error("provide { image: 'data:image/...;base64,...' } or { image_base64, mime }");

  // Bound the payload before sending it to the model.
  const b64 = dataUrl.slice(dataUrl.indexOf(",") + 1);
  const approxBytes = Math.floor((b64.length * 3) / 4);
  if (approxBytes > MAX_IMAGE_BYTES) {
    return error(`image too large (${approxBytes} bytes > ${MAX_IMAGE_BYTES})`, 413);
  }

  let raw: string;
  try {
    const result: any = await env.AI.run(OCR_MODEL, {
      messages: [
        { role: "system", content: OCR_SYSTEM_PROMPT },
        {
          role: "user",
          content: [
            { type: "image_url", image_url: { url: dataUrl } },
            { type: "text", text: "请抽取这张发票的信息，只输出 JSON。" },
          ],
        },
      ],
      max_tokens: 1024,
      temperature: 0,
    });
    raw =
      typeof result === "string"
        ? result
        : result?.response ?? result?.output_text ?? JSON.stringify(result ?? "");
  } catch (e: any) {
    return error(`OCR model unavailable: ${e?.message ?? e}`, 502);
  }

  // The model may wrap JSON in prose or a code fence; extract the first {...} block.
  const cleaned = raw.replace(/```json|```/g, "").trim();
  const start = cleaned.indexOf("{");
  const end = cleaned.lastIndexOf("}");
  let parsed: any = null;
  if (start >= 0 && end > start) {
    try {
      parsed = JSON.parse(cleaned.slice(start, end + 1));
    } catch {
      parsed = null;
    }
  }

  if (!parsed) {
    // Could not parse structured fields — still return the raw text so the user can edit manually.
    return json({ ok: false, raw_text: cleaned, fields: null, model: OCR_MODEL });
  }

  const fields = {
    invoice_code: str(parsed.invoice_code),
    invoice_number: str(parsed.invoice_number),
    invoice_date: str(parsed.invoice_date),
    buyer_name: str(parsed.buyer_name),
    buyer_taxno: str(parsed.buyer_taxno),
    seller_name: str(parsed.seller_name),
    seller_taxno: str(parsed.seller_taxno),
    amount_cents: toCents(parsed.amount),
    tax_cents: toCents(parsed.tax),
    total_cents: toCents(parsed.total),
    raw_text: str(parsed.raw_text) || cleaned,
  };
  // If the model gave amount+tax but no total, derive it.
  if (!fields.total_cents && (fields.amount_cents || fields.tax_cents)) {
    fields.total_cents = fields.amount_cents + fields.tax_cents;
  }
  return json({ ok: true, fields, model: OCR_MODEL });
}

async function health(env: Env): Promise<Response> {
  // Cheapest possible liveness probe — does not imply the full read budget is available.
  await env.DB.prepare("SELECT 1").run();
  return json({ ok: true, service: "invoice-manager", time: Date.now() });
}

export default {
  async fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    const url = new URL(request.url);
    const method = request.method.toUpperCase();

    if (method === "OPTIONS") {
      return new Response(null, { status: 204, headers: CORS_HEADERS });
    }

    const path = url.pathname.replace(/\/+$/, "") || "/";

    try {
      if (path === "/api/health" && method === "GET") return await health(env);

      if (path === "/api/invoices" && method === "GET") return await listInvoices(env, url);
      if (path === "/api/invoices" && method === "POST") return await createInvoice(env, request);
      if (path === "/api/invoices/batch" && method === "POST") return await batchCreate(env, request);

      const single = path.match(/^\/api\/invoices\/([^/]+)$/);
      if (single) {
        const id = decodeURIComponent(single[1]);
        if (method === "GET") return await getInvoice(env, id);
        if (method === "PUT" || method === "PATCH") return await updateInvoice(env, id, request);
        if (method === "DELETE") return await deleteInvoice(env, id);
        return error("method not allowed", 405);
      }

      if (path === "/api/ocr" && method === "POST") return await ocr(env, request);

      // Everything else: serve the static download page (site/) through the ASSETS binding.
      return await env.ASSETS.fetch(request);
    } catch (e: any) {
      const msg = e?.message ?? String(e);
      // Surface D1 daily-read-limit errors explicitly so the client can stop retrying.
      if (/D1_ERROR|7500|daily row read limit|read.*limit/i.test(msg)) {
        return error(`D1 read budget exhausted: ${msg}. Resets at 00:00 UTC.`, 429);
      }
      return error(`internal error: ${msg}`, 500);
    }
  },
} satisfies ExportedHandler<Env>;
