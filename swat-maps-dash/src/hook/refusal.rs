//! `Refusal` is structured and cannot carry data (§5.1a).
//!
//! Every parameter type has a private constructor that only accepts what
//! the framework already knows is not data: a declared column, a page
//! variable key, a constant or a rule id the hook declared at registration.
//! The framework renders the text.

use crate::sql::GuardCode;

/// A column name the source declared or the query's output carries. Made
/// only by [`super::QueryShape::column`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaColumn(pub(crate) String);

impl SchemaColumn {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A variable key that exists on the page. Made only by
/// [`super::QueryShape::var`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarKey(pub(crate) String);

impl VarKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A threshold the hook declared at registration
/// ([`super::GuardedSource::threshold`]). Made only by
/// [`super::QueryShape::threshold`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Threshold(pub(crate) u64);

impl Threshold {
    pub fn get(self) -> u64 {
        self.0
    }
}

/// A rule id the hook declared at registration
/// ([`super::GuardedSource::rule`]), `[a-z0-9_]{1,40}`. Made only by
/// [`super::QueryShape::rule`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleId(pub(crate) String);

impl RuleId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a query was not run or its result withheld. Closed codes with
/// data-free parameters: no variant carries free text.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A column projected or grouped that the host forbids.
    ForbiddenColumn { column: SchemaColumn },
    /// A query on an aggregate-only source returns per-row data.
    NotAggregated,
    /// A whole result withheld by a minimum-size rule.
    BelowThreshold { rule: RuleId, threshold: Threshold },
    /// The hook's scope could not be established (fail-closed).
    ScopeUnavailable { rule: Option<RuleId> },
    /// The hook needs a variable narrowed first.
    VariableRequired { var: VarKey },
    ReaggregationNotAllowed,
    /// Any other host rule, named only by its id.
    PolicyDenied { rule: RuleId },
    /// The read guard refused the statement (C1, D1, D5).
    Guard { code: GuardCode },
}

impl Refusal {
    /// The stable code.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::ForbiddenColumn { .. } => "forbidden_column",
            Refusal::NotAggregated => "not_aggregated",
            Refusal::BelowThreshold { .. } => "below_threshold",
            Refusal::ScopeUnavailable { .. } => "scope_unavailable",
            Refusal::VariableRequired { .. } => "variable_required",
            Refusal::ReaggregationNotAllowed => "reaggregation_not_allowed",
            Refusal::PolicyDenied { .. } => "policy_denied",
            Refusal::Guard { code } => code.code(),
        }
    }

    /// The text shown on a widget and returned to the agent, in one of the
    /// six languages the SWAT apps share (`en`, `ja`, `vi`, `th`, `id`,
    /// `zh-hans`; see [`Lang::of`]); any other locale falls back to `en`.
    /// Built only from the code and its data-free parameters.
    ///
    /// The `vi`, `th`, `id` and `zh-hans` texts are MACHINE-DRAFTED and need
    /// a native speaker's review before customer release.
    pub fn render(&self, locale: &str) -> String {
        let lang = Lang::of(locale);
        match self {
            Refusal::ForbiddenColumn { column } => {
                let c = column.as_str();
                match lang {
                    Lang::En => format!("This view would show {c}, which this data source does not allow."),
                    Lang::Ja => format!("このビューは {c} を表示しますが、このデータソースでは許可されていません。"),
                    Lang::Vi => format!("Chế độ xem này sẽ hiển thị {c}, nguồn dữ liệu này không cho phép điều đó."),
                    Lang::Th => format!("มุมมองนี้จะแสดง {c} ซึ่งแหล่งข้อมูลนี้ไม่อนุญาต"),
                    Lang::Id => format!("Tampilan ini akan menampilkan {c}, yang tidak diizinkan oleh sumber data ini."),
                    Lang::Zh => format!("此视图将显示 {c}，但此数据源不允许。"),
                }
            }
            Refusal::NotAggregated => match lang {
                Lang::En => "This data source shows totals only, and this view would show single rows.",
                Lang::Ja => "このデータソースは集計結果のみ表示できます。",
                Lang::Vi => "Nguồn dữ liệu này chỉ hiển thị số tổng hợp, còn chế độ xem này sẽ hiển thị từng dòng.",
                Lang::Th => "แหล่งข้อมูลนี้แสดงได้เฉพาะยอดรวม แต่มุมมองนี้จะแสดงทีละแถว",
                Lang::Id => "Sumber data ini hanya menampilkan total, sedangkan tampilan ini akan menampilkan baris tunggal.",
                Lang::Zh => "此数据源仅显示汇总结果，而此视图将显示单行数据。",
            }
            .into(),
            Refusal::BelowThreshold { threshold, .. } => {
                let n = threshold.get();
                match lang {
                    Lang::En => format!("Hidden: fewer than {n} in this selection."),
                    Lang::Ja => format!("非表示: この選択は {n} 未満です。"),
                    Lang::Vi => format!("Đã ẩn: lựa chọn này có ít hơn {n}."),
                    Lang::Th => format!("ซ่อนไว้: รายการที่เลือกนี้มีน้อยกว่า {n}"),
                    Lang::Id => format!("Disembunyikan: kurang dari {n} dalam pilihan ini."),
                    Lang::Zh => format!("已隐藏：此选择少于 {n}。"),
                }
            }
            Refusal::ScopeUnavailable { .. } => match lang {
                Lang::En => "This data is not shown, because its access scope could not be checked.",
                Lang::Ja => "このデータの表示範囲を確認できないため、表示しません。",
                Lang::Vi => "Dữ liệu này không được hiển thị vì không thể kiểm tra phạm vi truy cập.",
                Lang::Th => "ไม่แสดงข้อมูลนี้ เนื่องจากตรวจสอบขอบเขตการเข้าถึงไม่ได้",
                Lang::Id => "Data ini tidak ditampilkan karena cakupan aksesnya tidak dapat diperiksa.",
                Lang::Zh => "无法确认此数据的访问范围，因此不予显示。",
            }
            .into(),
            Refusal::VariableRequired { var } => {
                let v = var.as_str();
                match lang {
                    Lang::En => format!("Choose one {v} first."),
                    Lang::Ja => format!("{v} を一つ選んでください。"),
                    Lang::Vi => format!("Hãy chọn một {v} trước."),
                    Lang::Th => format!("โปรดเลือก {v} หนึ่งรายการก่อน"),
                    Lang::Id => format!("Pilih satu {v} terlebih dahulu."),
                    Lang::Zh => format!("请先选择一个 {v}。"),
                }
            }
            Refusal::ReaggregationNotAllowed => match lang {
                Lang::En => "This data source does not allow its totals to be added up again.",
                Lang::Ja => "このデータソースは再集計できません。",
                Lang::Vi => "Nguồn dữ liệu này không cho phép cộng dồn lại các số tổng.",
                Lang::Th => "แหล่งข้อมูลนี้ไม่อนุญาตให้นำยอดรวมมารวมซ้ำ",
                Lang::Id => "Sumber data ini tidak mengizinkan totalnya dijumlahkan ulang.",
                Lang::Zh => "此数据源不允许对汇总结果再次汇总。",
            }
            .into(),
            Refusal::PolicyDenied { rule } => {
                let r = rule.as_str();
                match lang {
                    Lang::En => format!("This view is not allowed by the data source's rule {r}."),
                    Lang::Ja => format!("このデータソースの規則 {r} により表示できません。"),
                    Lang::Vi => format!("Chế độ xem này bị chặn bởi quy tắc {r} của nguồn dữ liệu."),
                    Lang::Th => format!("มุมมองนี้ไม่ได้รับอนุญาตตามกฎ {r} ของแหล่งข้อมูล"),
                    Lang::Id => format!("Tampilan ini tidak diizinkan oleh aturan {r} sumber data."),
                    Lang::Zh => format!("根据数据源的规则 {r}，此视图不允许显示。"),
                }
            }
            Refusal::Guard { code } => match code {
                GuardCode::TableFunction(f) => match lang {
                    Lang::En => format!("The table function {f}() is not allowed on this connection."),
                    Lang::Ja => format!("テーブル関数 {f}() はこの接続では使えません。"),
                    Lang::Vi => format!("Hàm bảng {f}() không được phép trên kết nối này."),
                    Lang::Th => format!("ไม่อนุญาตให้ใช้ฟังก์ชันตาราง {f}() กับการเชื่อมต่อนี้"),
                    Lang::Id => format!("Fungsi tabel {f}() tidak diizinkan pada koneksi ini."),
                    Lang::Zh => format!("此连接不允许使用表函数 {f}()。"),
                },
                _ => match lang {
                    Lang::En => "This SQL was not sent, because it is not one read-only SELECT.",
                    Lang::Ja => "このSQLは読み取り専用の一つのSELECTではないため、送信しません。",
                    Lang::Vi => "SQL này không được gửi vì không phải là một câu SELECT chỉ đọc duy nhất.",
                    Lang::Th => "ไม่ได้ส่ง SQL นี้ เนื่องจากไม่ใช่คำสั่ง SELECT แบบอ่านอย่างเดียวเพียงคำสั่งเดียว",
                    Lang::Id => "SQL ini tidak dikirim karena bukan satu SELECT hanya-baca.",
                    Lang::Zh => "此 SQL 未发送，因为它不是单条只读 SELECT 语句。",
                }
                .into(),
            },
        }
    }
}

/// The languages [`Refusal::render`] and [`super::NoHook`]'s suppressed
/// label speak: the set the SWAT platform's REST API declares (en, ja,
/// vi, th, id, zh-hans), which SWAT's apps all offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lang {
    En,
    Ja,
    Vi,
    Th,
    Id,
    Zh,
}

impl Lang {
    /// From a locale code or BCP 47 tag (`ja`, `ja-JP`, `zh-hans`,
    /// `zh-Hans-CN`, ...). Traditional Chinese (`zh-Hant`, `zh-TW`,
    /// `zh-HK`, `zh-MO`) and anything unknown is English: a Simplified text
    /// does not serve a Traditional reader.
    pub(crate) fn of(locale: &str) -> Lang {
        let tag = locale.to_ascii_lowercase().replace('_', "-");
        let mut parts = tag.split('-');
        match parts.next().unwrap_or("") {
            "ja" => Lang::Ja,
            "vi" => Lang::Vi,
            "th" => Lang::Th,
            "id" | "in" => Lang::Id,
            "zh" if !parts.any(|p| matches!(p, "hant" | "tw" | "hk" | "mo")) => Lang::Zh,
            _ => Lang::En,
        }
    }
}

/// `true` for a rule id: `[a-z0-9_]{1,40}`.
pub(crate) fn is_rule_id(s: &str) -> bool {
    (1..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
