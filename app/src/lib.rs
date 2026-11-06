//! 发票管理 — a cross-platform (macOS / Android) invoice manager built with
//! Native Shell (nsh.dev) in Rust.
//!
//! Features:
//!   * 录入发票：直接输入/粘贴文字（启发式解析），或选择图片 / 拍照（Android 系统
//!     相册选择器自带相机）后走云端 OCR（Cloudflare Worker + Workers AI）。
//!   * 发票列表：增、改、删，数据保存在 Cloudflare D1。
//!   * 多选发票 → 按指定格式拼接文字 → 复制到剪贴板。
//!
//! The Rust `App` component below is the root mounted on every platform: the
//! desktop binary (`src/main.rs`) and the prebuilt native runtime that
//! `nsh package --target android|macos` ships.

use std::rc::Rc;
use std::time::Duration;

use base64::Engine as _;
use nsh_base::{alert_dialog, input, theme};
use nsh_core::element::AnyElement;
use nsh_core::geometry::{DevicePixels, Percentage};
use nsh_core::picker::{PickRequest, PickedFile};
use nsh_core::prelude::*;
use nsh_core::storage::Storage;
use serde::{Deserialize, Serialize};

/// Storage key for the configurable backend base URL.
const API_BASE_KEY: &str = "api_base";
/// Default backend (Cloudflare Worker custom domain). Override at runtime in 设置,
/// or rebuild with the final deployed domain.
const DEFAULT_API_BASE: &str = "https://invoice.gakaki.com";

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// One invoice. Money is stored in cents (INTEGER) to match the D1 schema and
/// avoid float drift; timestamps are epoch milliseconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Invoice {
    #[serde(default)]
    id: String,
    #[serde(default)]
    invoice_code: String,
    #[serde(default)]
    invoice_number: String,
    #[serde(default)]
    invoice_date: String,
    #[serde(default)]
    buyer_name: String,
    #[serde(default)]
    buyer_taxno: String,
    #[serde(default)]
    seller_name: String,
    #[serde(default)]
    seller_taxno: String,
    #[serde(default)]
    amount_cents: i64,
    #[serde(default)]
    tax_cents: i64,
    #[serde(default)]
    total_cents: i64,
    #[serde(default)]
    raw_text: String,
    #[serde(default = "default_source")]
    source: String,
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    updated_at: i64,
}

fn default_source() -> String {
    "manual".to_string()
}

impl Default for Invoice {
    fn default() -> Self {
        Invoice {
            id: String::new(),
            invoice_code: String::new(),
            invoice_number: String::new(),
            invoice_date: String::new(),
            buyer_name: String::new(),
            buyer_taxno: String::new(),
            seller_name: String::new(),
            seller_taxno: String::new(),
            amount_cents: 0,
            tax_cents: 0,
            total_cents: 0,
            raw_text: String::new(),
            source: default_source(),
            created_at: 0,
            updated_at: 0,
        }
    }
}

impl Invoice {
    /// A short one-line summary used in list rows.
    fn summary(&self) -> String {
        let who = if !self.seller_name.is_empty() {
            self.seller_name.clone()
        } else if !self.buyer_name.is_empty() {
            self.buyer_name.clone()
        } else {
            "（未命名发票）".to_string()
        };
        format!("{} · {}", who, yuan(self.total_cents))
    }
}

/// Which screen the app shows.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Screen {
    List,
    Add,
    Detail,
    Output,
    Settings,
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

/// Cents → yuan with two decimals.
fn yuan(cents: i64) -> String {
    format!("{:.2}", cents as f64 / 100.0)
}

/// Parse a leading decimal number out of a string into cents.
fn parse_amount(s: &str) -> i64 {
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else if !num.is_empty() {
            break;
        }
    }
    let n: f64 = num.parse().unwrap_or(0.0);
    (n * 100.0).round() as i64
}

/// Minimal percent-encoding for a query value (no extra crate needed).
fn urlencoded(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// The agreed output template for a single invoice.
fn format_invoice(inv: &Invoice) -> String {
    format!(
        "发票代码：{code}\n\
         发票号码：{number}\n\
         开票日期：{date}\n\
         购买方：{buyer}（税号：{buyer_tax}）\n\
         销售方：{seller}（税号：{seller_tax}）\n\
         金额：{amount}　税额：{tax}　价税合计：{total}",
        code = inv.invoice_code,
        number = inv.invoice_number,
        date = inv.invoice_date,
        buyer = inv.buyer_name,
        buyer_tax = inv.buyer_taxno,
        seller = inv.seller_name,
        seller_tax = inv.seller_taxno,
        amount = yuan(inv.amount_cents),
        tax = yuan(inv.tax_cents),
        total = yuan(inv.total_cents),
    )
}

/// Concatenate several selected invoices with separators and a grand total.
fn format_selected(invs: &[Invoice]) -> String {
    if invs.is_empty() {
        return String::new();
    }
    let sep = "\n————————————\n";
    let body: Vec<String> = invs.iter().map(format_invoice).collect();
    let total: i64 = invs.iter().map(|i| i.total_cents).sum();
    format!(
        "{body}\n————————————\n共 {n} 张发票，价税合计：{total}",
        body = body.join(sep),
        n = invs.len(),
        total = yuan(total),
    )
}

/// Best-effort heuristic parse of pasted invoice text into fields. The user can
/// always correct the result in the form.
fn parse_invoice_text(raw: &str) -> Invoice {
    let mut inv = Invoice::default();
    inv.raw_text = raw.to_string();
    inv.source = "text".to_string();
    // Normalise the full-width colon and drop spaces so keyword scanning is stable.
    let text = raw.replace('：', ":").replace(' ', "").replace('\u{3000}', "");

    fn value_after(text: &str, keys: &[&str]) -> String {
        for k in keys {
            if let Some(idx) = text.find(k) {
                let rest = &text[idx + k.len()..];
                let rest = rest.trim_start_matches(':');
                let end = rest
                    .find(|c: char| c == '\n' || c == '\r')
                    .unwrap_or(rest.len());
                let v = rest[..end].trim();
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
        String::new()
    }

    inv.invoice_code = value_after(&text, &["发票代码"]);
    inv.invoice_number = value_after(&text, &["发票号码"]);
    inv.invoice_date = value_after(&text, &["开票日期"]);
    inv.buyer_name = value_after(&text, &["购买方名称", "购买方"]);
    inv.buyer_taxno = value_after(&text, &["购买方纳税人识别号", "购买方税号"]);
    inv.seller_name = value_after(&text, &["销售方名称", "销售方"]);
    inv.seller_taxno = value_after(&text, &["销售方纳税人识别号", "销售方税号"]);
    inv.amount_cents = parse_amount(&value_after(&text, &["金额"]));
    inv.tax_cents = parse_amount(&value_after(&text, &["税额"]));
    inv.total_cents = parse_amount(&value_after(&text, &["价税合计", "价税", "合计"]));
    if inv.total_cents == 0 {
        inv.total_cents = inv.amount_cents + inv.tax_cents;
    }
    inv
}

// ---------------------------------------------------------------------------
// Backend API client (Cloudflare Worker + D1)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ListResponse {
    #[serde(default)]
    invoices: Vec<Invoice>,
}

#[derive(Deserialize)]
struct InvoiceResponse {
    invoice: Invoice,
}

#[derive(Clone, Default, Deserialize)]
struct OcrFields {
    #[serde(default)]
    invoice_code: String,
    #[serde(default)]
    invoice_number: String,
    #[serde(default)]
    invoice_date: String,
    #[serde(default)]
    buyer_name: String,
    #[serde(default)]
    buyer_taxno: String,
    #[serde(default)]
    seller_name: String,
    #[serde(default)]
    seller_taxno: String,
    #[serde(default)]
    amount_cents: i64,
    #[serde(default)]
    tax_cents: i64,
    #[serde(default)]
    total_cents: i64,
    #[serde(default)]
    raw_text: String,
}

#[derive(Deserialize)]
struct OcrResponse {
    #[serde(default)]
    fields: Option<OcrFields>,
    #[serde(default)]
    raw_text: Option<String>,
}

fn base_url(base: &str) -> String {
    base.trim_end_matches('/').to_string()
}

async fn api_list(base: String, q: String) -> Result<Vec<Invoice>, String> {
    let root = base_url(&base);
    let url = if q.trim().is_empty() {
        format!("{root}/api/invoices?limit=200")
    } else {
        format!("{root}/api/invoices?limit=200&q={}", urlencoded(q.trim()))
    };
    let resp = fetch(url).await?;
    if !resp.ok() {
        return Err(format!("列表请求失败 HTTP {}", resp.status()));
    }
    let body = resp.text().to_string();
    let parsed: ListResponse = serde_json::from_str(&body).map_err(|e| format!("解析失败: {e}"))?;
    Ok(parsed.invoices)
}

async fn api_save(base: String, inv: Invoice) -> Result<Invoice, String> {
    let root = base_url(&base);
    let is_new = inv.id.trim().is_empty();
    let body = serde_json::to_string(&inv).map_err(|e| e.to_string())?;
    let (url, method) = if is_new {
        (format!("{root}/api/invoices"), "POST")
    } else {
        (
            format!("{root}/api/invoices/{}", urlencoded(&inv.id)),
            "PUT",
        )
    };
    let req = HttpRequest::new(method, url)
        .header("Content-Type", "application/json")
        .body(body);
    let resp = fetch(req).await?;
    if !resp.ok() {
        return Err(format!("保存失败 HTTP {}", resp.status()));
    }
    let text = resp.text().to_string();
    let parsed: InvoiceResponse =
        serde_json::from_str(&text).map_err(|e| format!("解析失败: {e}"))?;
    Ok(parsed.invoice)
}

async fn api_delete(base: String, id: String) -> Result<(), String> {
    let root = base_url(&base);
    let req = HttpRequest::new(
        "DELETE",
        format!("{root}/api/invoices/{}", urlencoded(&id)),
    );
    let resp = fetch(req).await?;
    if !resp.ok() {
        return Err(format!("删除失败 HTTP {}", resp.status()));
    }
    Ok(())
}

async fn api_ocr(base: String, data_url: String) -> Result<OcrFields, String> {
    let root = base_url(&base);
    let body = serde_json::json!({ "image": data_url }).to_string();
    let req =
        HttpRequest::post(format!("{root}/api/ocr"), body).header("Content-Type", "application/json");
    let resp = fetch(req).await?;
    if !resp.ok() {
        return Err(format!("OCR 请求失败 HTTP {}", resp.status()));
    }
    let text = resp.text().to_string();
    let parsed: OcrResponse = serde_json::from_str(&text).map_err(|e| format!("解析失败: {e}"))?;
    match parsed.fields {
        Some(f) => Ok(f),
        None => Ok(OcrFields {
            raw_text: parsed.raw_text.unwrap_or_default(),
            ..Default::default()
        }),
    }
}

/// Apply OCR fields onto a draft invoice.
fn apply_ocr(inv: &mut Invoice, f: OcrFields) {
    inv.invoice_code = f.invoice_code;
    inv.invoice_number = f.invoice_number;
    inv.invoice_date = f.invoice_date;
    inv.buyer_name = f.buyer_name;
    inv.buyer_taxno = f.buyer_taxno;
    inv.seller_name = f.seller_name;
    inv.seller_taxno = f.seller_taxno;
    inv.amount_cents = f.amount_cents;
    inv.tax_cents = f.tax_cents;
    inv.total_cents = if f.total_cents != 0 {
        f.total_cents
    } else {
        f.amount_cents + f.tax_cents
    };
    if !f.raw_text.is_empty() {
        inv.raw_text = f.raw_text;
    }
    inv.source = "ocr".to_string();
}

// ---------------------------------------------------------------------------
// Shared app state
// ---------------------------------------------------------------------------

/// All the reactive state the screens share. Every field is a cheap `Copy`
/// signal handle, so it can be passed into any screen or closure.
#[derive(Clone, Copy)]
struct State {
    api_base: Signal<String>,
    invoices: Signal<Rc<Vec<Invoice>>>,
    selected: Signal<Rc<Vec<String>>>,
    screen: Signal<Screen>,
    /// The invoice being added / edited / viewed (`None` only on the list).
    editing: Signal<Option<Invoice>>,
    /// Add-screen input mode: false = 文字, true = 图片.
    image_mode: Signal<bool>,
    busy: Signal<bool>,
    toast: Signal<String>,
}

impl State {
    fn notify(&self, msg: impl Into<String>) {
        self.toast.set(msg.into());
        let toast = self.toast;
        set_timeout(Duration::from_millis(2800), move || toast.set(String::new()));
    }

    fn selected_invoices(&self) -> Vec<Invoice> {
        let sel = self.selected.get();
        self.invoices
            .get()
            .iter()
            .filter(|i| sel.contains(&i.id))
            .cloned()
            .collect()
    }

    fn toggle_select(&self, id: &str) {
        let id = id.to_string();
        self.selected.update(|s| {
            let mut v = (**s).clone();
            if let Some(pos) = v.iter().position(|x| *x == id) {
                v.remove(pos);
            } else {
                v.push(id);
            }
            *s = Rc::new(v);
        });
    }

    fn go(&self, screen: Screen) {
        self.screen.set(screen);
    }

    /// Reload the invoice list from D1.
    fn reload(&self) {
        let base = self.api_base.get();
        let invoices = self.invoices;
        let busy = self.busy;
        let toast = self.toast;
        busy.set(true);
        spawn(async move {
            match api_list(base, String::new()).await {
                Ok(list) => {
                    invoices.set(Rc::new(list));
                    toast.set(String::new());
                }
                Err(e) => toast.set(e),
            }
            busy.set(false);
        });
    }

    /// Save the draft in `editing`, then refresh and return to the list.
    fn save_draft(&self) {
        let Some(draft) = self.editing.get() else {
            return;
        };
        if draft.invoice_number.trim().is_empty() && draft.seller_name.trim().is_empty() {
            self.notify("请至少填写发票号码或销售方");
            return;
        }
        let base = self.api_base.get();
        let st = *self;
        st.busy.set(true);
        spawn(async move {
            match api_save(base, draft).await {
                Ok(saved) => {
                    st.invoices.update(|list| {
                        let mut v = (**list).clone();
                        if let Some(pos) = v.iter().position(|i| i.id == saved.id) {
                            v[pos] = saved.clone();
                        } else {
                            v.insert(0, saved.clone());
                        }
                        *list = Rc::new(v);
                    });
                    st.editing.set(None);
                    st.busy.set(false);
                    st.notify("已保存");
                    st.go(Screen::List);
                }
                Err(e) => {
                    st.busy.set(false);
                    st.notify(e);
                }
            }
        });
    }

    /// Delete an invoice after confirmation.
    fn delete_current(&self) {
        let Some(inv) = self.editing.get() else {
            return;
        };
        if inv.id.trim().is_empty() {
            self.editing.set(None);
            self.go(Screen::List);
            return;
        }
        let chosen = alert_dialog()
            .title("删除这张发票？")
            .description(format!("发票号码：{}", inv.invoice_number))
            .cancel("cancel", "取消")
            .destructive("delete", "删除")
            .close_on_escape(true)
            .close_on_backdrop_press(true)
            .show();
        let st = *self;
        let id = inv.id.clone();
        let base = self.api_base.get();
        spawn(async move {
            let confirmed = chosen
                .await
                .is_ok_and(|r| r.action.as_deref() == Some("delete"));
            if !confirmed {
                return;
            }
            st.busy.set(true);
            match api_delete(base, id.clone()).await {
                Ok(()) => {
                    st.invoices.update(|list| {
                        let mut v = (**list).clone();
                        v.retain(|i| i.id != id);
                        *list = Rc::new(v);
                    });
                    st.selected.update(|s| {
                        let mut v = (**s).clone();
                        v.retain(|x| *x != id);
                        *s = Rc::new(v);
                    });
                    st.editing.set(None);
                    st.busy.set(false);
                    st.notify("已删除");
                    st.go(Screen::List);
                }
                Err(e) => {
                    st.busy.set(false);
                    st.notify(e);
                }
            }
        });
    }

    /// Pick an image (Android system picker has a built-in camera; desktop opens
    /// a file dialog), run OCR, and fill the draft.
    fn pick_and_ocr(&self) {
        let mut request = PickRequest::new(false, 1);
        request.max_dimension = Some(DevicePixels(2048));
        request.quality = Some(Percentage(0.85));
        let picked = pick_images(request);
        let st = *self;
        let base = self.api_base.get();
        st.busy.set(true);
        spawn(async move {
            let files: Vec<PickedFile> = picked.await;
            let Some(file) = files.into_iter().next() else {
                st.busy.set(false);
                return; // user cancelled
            };
            let mime = file.mime.as_str().to_string();
            let encoded = base64::engine::general_purpose::STANDARD.encode(file.data.to_vec());
            let data_url = format!("data:{mime};base64,{encoded}");
            st.toast.set("正在识别发票…".to_string());
            match api_ocr(base, data_url).await {
                Ok(fields) => {
                    st.editing.update(|e| {
                        let inv = e.get_or_insert_with(Invoice::default);
                        apply_ocr(inv, fields);
                    });
                    st.busy.set(false);
                    st.notify("识别完成，请核对字段");
                }
                Err(e) => {
                    st.busy.set(false);
                    st.notify(e);
                }
            }
        });
    }

    /// Parse the draft's raw text into fields (文字 mode).
    fn parse_draft_text(&self) {
        let Some(inv) = self.editing.get() else {
            return;
        };
        if inv.raw_text.trim().is_empty() {
            self.notify("请先粘贴或输入发票文字");
            return;
        }
        let parsed = parse_invoice_text(&inv.raw_text);
        self.editing.update(|e| {
            let inv = e.get_or_insert_with(Invoice::default);
            let mut p = parsed.clone();
            p.raw_text = inv.raw_text.clone();
            p.id = inv.id.clone();
            p.created_at = inv.created_at;
            *inv = p;
        });
        self.notify("已解析，请核对字段");
    }
}

// ---------------------------------------------------------------------------
// Small UI building blocks
// ---------------------------------------------------------------------------

/// A pressable button. `fill` stretches it to its parent's width.
fn press_button(
    label: impl Into<String>,
    on_press: impl Fn() + 'static,
    primary: bool,
    fill: bool,
) -> AnyElement {
    let label = label.into();
    let mut v = view()
        .px_4()
        .py_3()
        .rounded(px(10.0))
        .border_curve(BorderCurve::Continuous)
        .items_center()
        .justify_center()
        .active(|s| s.opacity(0.7))
        .on_press(on_press)
        .child(
            text(label)
                .text_size(px(15.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(move || {
                    if primary {
                        theme().primary_foreground
                    } else {
                        theme().foreground
                    }
                }),
        );
    if fill {
        v = v.w_full();
    }
    if primary {
        v = v.bg(move || theme().primary);
    } else {
        v = v
            .bg(move || theme().secondary)
            .border_1()
            .border_color(move || theme().border);
    }
    v.into_element()
}

/// A labelled single-line text field bound to a string field of the draft.
fn text_field(
    label: &str,
    editing: Signal<Option<Invoice>>,
    get: fn(&Invoice) -> String,
    set: fn(&mut Invoice, String),
) -> AnyElement {
    let label = label.to_string();
    view()
        .gap_1()
        .child(
            text(label)
                .text_size(px(12.0))
                .text_color(move || theme().muted_foreground),
        )
        .child(
            input()
                .value(move || editing.with(|e| e.as_ref().map(get).unwrap_or_default()))
                .on_change(move |t: &str| {
                    editing.update(|e| {
                        if let Some(i) = e.as_mut() {
                            set(i, t.to_string());
                        }
                    })
                })
                .text_size(px(15.0))
                .text_color(move || theme().foreground)
                .placeholder_color(rgb(0x8a8f98))
                .px_3()
                .py_2()
                .rounded(px(8.0))
                .border_1()
                .border_color(move || theme().border)
                .bg(move || theme().card),
        )
        .into_element()
}

/// A labelled money field: shows yuan, stores cents.
fn money_field(
    label: &str,
    editing: Signal<Option<Invoice>>,
    get: fn(&Invoice) -> i64,
    set: fn(&mut Invoice, i64),
) -> AnyElement {
    let label = label.to_string();
    view()
        .gap_1()
        .child(
            text(label)
                .text_size(px(12.0))
                .text_color(move || theme().muted_foreground),
        )
        .child(
            input()
                .value(move || editing.with(|e| e.as_ref().map(|i| yuan(get(i))).unwrap_or_default()))
                .on_change(move |t: &str| {
                    let cents = parse_amount(t);
                    editing.update(|e| {
                        if let Some(i) = e.as_mut() {
                            set(i, cents);
                        }
                    })
                })
                .text_size(px(15.0))
                .text_color(move || theme().foreground)
                .placeholder_color(rgb(0x8a8f98))
                .px_3()
                .py_2()
                .rounded(px(8.0))
                .border_1()
                .border_color(move || theme().border)
                .bg(move || theme().card),
        )
        .into_element()
}

fn header(title: &str, st: State, back: bool) -> AnyElement {
    let title = title.to_string();
    let mut h = view()
        .flex_row()
        .items_center()
        .gap_3()
        .px_4()
        .py_3()
        .border_b_1()
        .border_b_color(move || theme().border)
        .bg(move || theme().background);
    if back {
        h = h.child(
            text("‹ 返回")
                .text_size(px(16.0))
                .text_color(move || theme().primary)
                .on_press(move || st.go(Screen::List)),
        );
    }
    h = h.child(
        text(title)
            .flex_1()
            .text_size(px(18.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(move || theme().foreground),
    );
    h.into_element()
}

/// A transient status bar pinned to the bottom.
fn toast_bar(st: State) -> AnyElement {
    when(
        move || !st.toast.get().is_empty(),
        move || {
            view()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .px_4()
                .py_3()
                .bg(move || theme().foreground)
                .child(
                    text(move || st.toast.get())
                        .text_size(px(14.0))
                        .text_color(move || theme().background),
                )
        },
    )
    .into_element()
}

fn busy_overlay(st: State) -> AnyElement {
    when(
        move || st.busy.get(),
        move || {
            view()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .items_center()
                .justify_center()
                .child(
                    view()
                        .px_5()
                        .py_4()
                        .rounded(px(12.0))
                        .bg(move || theme().card)
                        .border_1()
                        .border_color(move || theme().border)
                        .child(
                            text("处理中…")
                                .text_size(px(15.0))
                                .text_color(move || theme().foreground),
                        ),
                )
        },
    )
    .into_element()
}

// ---------------------------------------------------------------------------
// Screens — each builder constructs fresh elements, so they are safe to call
// from inside reactive `dynamic` / `when` closures (which may re-run).
// ---------------------------------------------------------------------------

fn invoice_row(inv: Invoice, st: State) -> AnyElement {
    let id = inv.id.clone();
    let summary = inv.summary();
    let number = inv.invoice_number.clone();
    let date = inv.invoice_date.clone();

    view()
        .flex_row()
        .items_center()
        .gap_3()
        .p_3()
        .rounded(px(12.0))
        .border_curve(BorderCurve::Continuous)
        .bg(move || theme().card)
        .border_1()
        .border_color(move || theme().border)
        // Selection checkbox.
        .child(
            view()
                .size(px(26.0))
                .rounded(px(7.0))
                .border_curve(BorderCurve::Continuous)
                .items_center()
                .justify_center()
                .border_1()
                .border_color(move || theme().border)
                .test_id(&format!("select-{number}"))
                .bg({
                    let id = id.clone();
                    move || {
                        if st.selected.with(|s| s.contains(&id)) {
                            theme().primary
                        } else {
                            theme().card
                        }
                    }
                })
                .on_press({
                    let id = id.clone();
                    move || st.toggle_select(&id)
                })
                .child(when(
                    {
                        let id = id.clone();
                        move || st.selected.with(|s| s.contains(&id))
                    },
                    move || {
                        text("✓")
                            .text_size(px(15.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(move || theme().primary_foreground)
                    },
                )),
        )
        // Body: tap to open detail.
        .child(
            view()
                .flex_1()
                .gap_1()
                .on_press({
                    let inv = inv.clone();
                    move || {
                        st.editing.set(Some(inv.clone()));
                        st.go(Screen::Detail);
                    }
                })
                .child(
                    text(summary)
                        .number_of_lines(1)
                        .text_size(px(15.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(move || theme().foreground),
                )
                .child(
                    text(move || {
                        let mut parts = Vec::new();
                        if !number.is_empty() {
                            parts.push(format!("No.{number}"));
                        }
                        if !date.is_empty() {
                            parts.push(date.clone());
                        }
                        if parts.is_empty() {
                            "（无号码/日期）".to_string()
                        } else {
                            parts.join(" · ")
                        }
                    })
                    .number_of_lines(1)
                    .text_size(px(12.0))
                    .text_color(move || theme().muted_foreground),
                ),
        )
        .child(
            text("›")
                .text_size(px(20.0))
                .text_color(move || theme().muted_foreground),
        )
        .into_element()
}

fn list_screen(st: State) -> AnyElement {
    let rows = dynamic(move || {
        let list = st.invoices.get();
        if list.is_empty() {
            return view()
                .flex_1()
                .items_center()
                .justify_center()
                .p_6()
                .child(
                    text("还没有发票\n点击右上角「添加」录入第一张")
                        .text_center()
                        .text_size(px(15.0))
                        .text_color(move || theme().muted_foreground),
                )
                .into_element();
        }
        let children: Vec<AnyElement> = list.iter().map(|inv| invoice_row(inv.clone(), st)).collect();
        scroll()
            .flex_1()
            .child(view().gap_2().p_3().child(children))
            .into_element()
    });

    let sel_count = move || st.selected.get().len();

    view()
        .flex_1()
        .child(header("发票管理", st, false))
        // Top action bar.
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_3()
                .px_4()
                .py_2()
                .child(
                    text("刷新")
                        .text_size(px(14.0))
                        .text_color(move || theme().primary)
                        .on_press(move || st.reload()),
                )
                .child(
                    text("设置")
                        .text_size(px(14.0))
                        .text_color(move || theme().muted_foreground)
                        .on_press(move || st.go(Screen::Settings)),
                )
                .child(view().flex_1())
                .child(press_button(
                    "添加",
                    move || {
                        st.editing.set(Some(Invoice::default()));
                        st.image_mode.set(false);
                        st.go(Screen::Add);
                    },
                    true,
                    false,
                )),
        )
        .child(rows)
        // Bottom selection / output bar.
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_3()
                .px_4()
                .py_3()
                .border_t_1()
                .border_t_color(move || theme().border)
                .bg(move || theme().background)
                .child(
                    text(move || {
                        let n = sel_count();
                        if n == 0 {
                            "勾选发票后可拼接输出".to_string()
                        } else {
                            format!("已选 {n} 张")
                        }
                    })
                    .flex_1()
                    .text_size(px(14.0))
                    .text_color(move || theme().muted_foreground),
                )
                .child(when(move || sel_count() > 0, move || {
                    press_button("输出选中", move || st.go(Screen::Output), true, false)
                }))
                .child(when(move || sel_count() > 0, move || {
                    text("清空")
                        .text_size(px(14.0))
                        .text_color(move || theme().muted_foreground)
                        .on_press(move || st.selected.set(Rc::new(Vec::new())))
                })),
        )
        .into_element()
}

fn add_mode_toggle(st: State) -> AnyElement {
    view()
        .flex_row()
        .gap_2()
        .child(
            text("文字输入")
                .flex_1()
                .text_center()
                .text_size(px(14.0))
                .py_2()
                .rounded(px(8.0))
                .bg(move || {
                    if st.image_mode.get() {
                        theme().secondary
                    } else {
                        theme().primary
                    }
                })
                .text_color(move || {
                    if st.image_mode.get() {
                        theme().foreground
                    } else {
                        theme().primary_foreground
                    }
                })
                .on_press(move || st.image_mode.set(false)),
        )
        .child(
            text("图片识别")
                .flex_1()
                .text_center()
                .text_size(px(14.0))
                .py_2()
                .rounded(px(8.0))
                .bg(move || {
                    if st.image_mode.get() {
                        theme().primary
                    } else {
                        theme().secondary
                    }
                })
                .text_color(move || {
                    if st.image_mode.get() {
                        theme().primary_foreground
                    } else {
                        theme().foreground
                    }
                })
                .on_press(move || st.image_mode.set(true)),
        )
        .into_element()
}

fn add_text_mode(st: State) -> AnyElement {
    let editing = st.editing;
    view()
        .gap_2()
        .child(
            input()
                .multiline(true)
                .value(move || {
                    editing.with(|e| e.as_ref().map(|i| i.raw_text.clone()).unwrap_or_default())
                })
                .on_change(move |t: &str| {
                    editing.update(|e| {
                        if let Some(i) = e.as_mut() {
                            i.raw_text = t.to_string();
                        }
                    })
                })
                .placeholder("粘贴或输入发票文字，然后点「解析字段」")
                .text_size(px(14.0))
                .text_color(move || theme().foreground)
                .placeholder_color(rgb(0x8a8f98))
                .h(px(120.0))
                .px_3()
                .py_2()
                .rounded(px(8.0))
                .border_1()
                .border_color(move || theme().border)
                .bg(move || theme().card),
        )
        .child(press_button("解析字段", move || st.parse_draft_text(), false, true))
        .into_element()
}

fn add_image_mode(st: State) -> AnyElement {
    view()
        .gap_2()
        .child(
            text("选择一张图片，或在 Android 上用系统选择器自带的相机拍摄；识别后可在下方核对字段。")
                .text_size(px(13.0))
                .text_color(move || theme().muted_foreground),
        )
        .child(press_button(
            "选择图片 / 拍照并识别",
            move || st.pick_and_ocr(),
            true,
            true,
        ))
        .into_element()
}

fn add_fields(st: State) -> AnyElement {
    let editing = st.editing;
    view()
        .gap_3()
        .child(text_field("发票代码", editing, |i| i.invoice_code.clone(), |i, v| {
            i.invoice_code = v
        }))
        .child(text_field("发票号码", editing, |i| i.invoice_number.clone(), |i, v| {
            i.invoice_number = v
        }))
        .child(text_field("开票日期 (YYYY-MM-DD)", editing, |i| i.invoice_date.clone(), |i, v| {
            i.invoice_date = v
        }))
        .child(text_field("购买方名称", editing, |i| i.buyer_name.clone(), |i, v| {
            i.buyer_name = v
        }))
        .child(text_field("购买方税号", editing, |i| i.buyer_taxno.clone(), |i, v| {
            i.buyer_taxno = v
        }))
        .child(text_field("销售方名称", editing, |i| i.seller_name.clone(), |i, v| {
            i.seller_name = v
        }))
        .child(text_field("销售方税号", editing, |i| i.seller_taxno.clone(), |i, v| {
            i.seller_taxno = v
        }))
        .child(
            view()
                .flex_row()
                .gap_2()
                .child(
                    view()
                        .flex_1()
                        .child(money_field("金额(元)", editing, |i| i.amount_cents, |i, v| {
                            i.amount_cents = v
                        })),
                )
                .child(
                    view()
                        .flex_1()
                        .child(money_field("税额(元)", editing, |i| i.tax_cents, |i, v| {
                            i.tax_cents = v
                        })),
                )
                .child(
                    view()
                        .flex_1()
                        .child(money_field("价税合计(元)", editing, |i| i.total_cents, |i, v| {
                            i.total_cents = v
                        })),
                ),
        )
        .into_element()
}

fn add_screen(st: State) -> AnyElement {
    let is_new = move || {
        st.editing
            .with(|e| e.as_ref().map(|i| i.id.trim().is_empty()).unwrap_or(true))
    };
    let title = if is_new() { "添加发票" } else { "编辑发票" };

    view()
        .flex_1()
        .child(header(title, st, true))
        .child(
            scroll().flex_1().child(
                view()
                    .gap_4()
                    .p_4()
                    // Only offer the input-mode toggle for a brand-new invoice.
                    .child(when(is_new, move || add_mode_toggle(st)))
                    .child(dynamic(move || {
                        if st.image_mode.get() {
                            add_image_mode(st)
                        } else {
                            add_text_mode(st)
                        }
                    }))
                    .child(add_fields(st))
                    .child(press_button("保存", move || st.save_draft(), true, true))
                    .pb(px(40.0)),
            ),
        )
        .into_element()
}

fn detail_row(label: &'static str, editing: Signal<Option<Invoice>>, get: fn(&Invoice) -> String) -> AnyElement {
    view()
        .flex_row()
        .gap_2()
        .py_1()
        .child(
            text(label)
                .w(px(96.0))
                .text_size(px(13.0))
                .text_color(move || theme().muted_foreground),
        )
        .child(
            text(move || editing.with(|e| e.as_ref().map(get).unwrap_or_default()))
                .flex_1()
                .selectable(true)
                .text_size(px(14.0))
                .text_color(move || theme().foreground),
        )
        .into_element()
}

fn detail_screen(st: State) -> AnyElement {
    let editing = st.editing;
    view()
        .flex_1()
        .child(header("发票详情", st, true))
        .child(
            scroll().flex_1().child(
                view()
                    .gap_2()
                    .p_4()
                    .child(detail_row("发票代码", editing, |i| i.invoice_code.clone()))
                    .child(detail_row("发票号码", editing, |i| i.invoice_number.clone()))
                    .child(detail_row("开票日期", editing, |i| i.invoice_date.clone()))
                    .child(detail_row("购买方", editing, |i| i.buyer_name.clone()))
                    .child(detail_row("购买方税号", editing, |i| i.buyer_taxno.clone()))
                    .child(detail_row("销售方", editing, |i| i.seller_name.clone()))
                    .child(detail_row("销售方税号", editing, |i| i.seller_taxno.clone()))
                    .child(detail_row("金额(元)", editing, |i| yuan(i.amount_cents)))
                    .child(detail_row("税额(元)", editing, |i| yuan(i.tax_cents)))
                    .child(detail_row("价税合计(元)", editing, |i| yuan(i.total_cents)))
                    .child(detail_row("来源", editing, |i| i.source.clone()))
                    .child(
                        view()
                            .gap_1()
                            .pt_2()
                            .child(
                                text("原始文本")
                                    .text_size(px(13.0))
                                    .text_color(move || theme().muted_foreground),
                            )
                            .child(
                                text(move || {
                                    editing
                                        .with(|e| e.as_ref().map(|i| i.raw_text.clone()).unwrap_or_default())
                                })
                                .selectable(true)
                                .text_size(px(13.0))
                                .text_color(move || theme().foreground)
                                .p_3()
                                .rounded(px(8.0))
                                .bg(move || theme().muted),
                            ),
                    )
                    .child(
                        view()
                            .flex_row()
                            .gap_2()
                            .pt_3()
                            .child(
                                view().flex_1().child(press_button(
                                    "编辑",
                                    move || {
                                        st.image_mode.set(false);
                                        st.go(Screen::Add);
                                    },
                                    true,
                                    true,
                                )),
                            )
                            .child(
                                view()
                                    .flex_1()
                                    .child(press_button("删除", move || st.delete_current(), false, true)),
                            ),
                    )
                    .pb(px(40.0)),
            ),
        )
        .into_element()
}

fn output_screen(st: State) -> AnyElement {
    view()
        .flex_1()
        .child(header("拼接输出", st, true))
        .child(
            scroll().flex_1().child(
                view()
                    .gap_3()
                    .p_4()
                    .child(
                        text(move || {
                            let n = st.selected.get().len();
                            format!("已选 {n} 张发票，按约定格式拼接如下：")
                        })
                        .text_size(px(13.0))
                        .text_color(move || theme().muted_foreground),
                    )
                    .child(
                        text(move || format_selected(&st.selected_invoices()))
                            .selectable(true)
                            .text_size(px(14.0))
                            .text_color(move || theme().foreground)
                            .p_4()
                            .rounded(px(10.0))
                            .border_1()
                            .border_color(move || theme().border)
                            .bg(move || theme().card),
                    )
                    .child(press_button(
                        "复制到剪贴板",
                        move || {
                            let out = format_selected(&st.selected_invoices());
                            if out.is_empty() {
                                st.notify("没有选中的发票");
                                return;
                            }
                            set_clipboard(out);
                            st.notify("已复制到剪贴板");
                        },
                        true,
                        true,
                    ))
                    .child(press_button(
                        "清空选择",
                        move || {
                            st.selected.set(Rc::new(Vec::new()));
                            st.go(Screen::List);
                        },
                        false,
                        true,
                    ))
                    .pb(px(40.0)),
            ),
        )
        .into_element()
}

fn settings_screen(st: State) -> AnyElement {
    let api_base = st.api_base;
    view()
        .flex_1()
        .child(header("设置", st, true))
        .child(
            scroll().flex_1().child(
                view()
                    .gap_3()
                    .p_4()
                    .child(
                        text("后端地址 (Cloudflare Worker)")
                            .text_size(px(12.0))
                            .text_color(move || theme().muted_foreground),
                    )
                    .child(
                        input()
                            .value(move || api_base.get())
                            .on_change(move |t: &str| api_base.set(t.to_string()))
                            .placeholder("https://invoice.gakaki.com")
                            .auto_capitalize(AutoCapitalize::None)
                            .auto_correct(false)
                            .text_size(px(14.0))
                            .text_color(move || theme().foreground)
                            .px_3()
                            .py_2()
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(move || theme().border)
                            .bg(move || theme().card),
                    )
                    .child(press_button(
                        "保存并刷新",
                        move || {
                            let base = st.api_base.get();
                            let _ = Storage::set_item(API_BASE_KEY, base.trim().to_string());
                            st.notify("已保存后端地址");
                            st.reload();
                            st.go(Screen::List);
                        },
                        true,
                        true,
                    ))
                    .child(
                        text("说明：发票数据保存在 Cloudflare D1；OCR 由 Cloudflare Worker 调用 Workers AI 视觉模型完成。地址修改后会保存在本机。")
                            .text_size(px(12.0))
                            .text_color(move || theme().muted_foreground),
                    )
                    .pb(px(40.0)),
            ),
        )
        .into_element()
}

// ---------------------------------------------------------------------------
// Root component
// ---------------------------------------------------------------------------

/// The components available to TSX through `app/native`.
#[native_module("app/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}

/// The application's root component, mounted on every platform.
#[component]
pub fn App() -> impl IntoElement {
    let st = State {
        api_base: signal(
            Storage::get_item(API_BASE_KEY)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
        ),
        invoices: signal(Rc::new(Vec::<Invoice>::new())),
        selected: signal(Rc::new(Vec::<String>::new())),
        screen: signal(Screen::List),
        editing: signal(None::<Invoice>),
        image_mode: signal(false),
        busy: signal(false),
        toast: signal(String::new()),
    };

    // Load the list once on launch.
    st.reload();

    view()
        .flex_1()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .p_safe_area()
        .child(dynamic(move || match st.screen.get() {
            Screen::List => list_screen(st),
            Screen::Add => add_screen(st),
            Screen::Detail => detail_screen(st),
            Screen::Output => output_screen(st),
            Screen::Settings => settings_screen(st),
        }))
        .child(busy_overlay(st))
        .child(toast_bar(st))
}
