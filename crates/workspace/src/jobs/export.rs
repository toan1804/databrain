use chrono::{Local, TimeZone};

use super::FileFormat;

/// File name of an export step at `now_ms`: `{step}`, `{job}`, `{date}`
/// (YYYY-MM-DD), `{time}` (HHMMSS) replaced, local time; characters not
/// allowed in file names become `_`; the format's extension is added when
/// missing. Empty = `{step}_{date}_{time}`.
pub fn export_file_name(template: Option<&str>, step: &str, job: &str, format: FileFormat, now_ms: i64) -> String {
    let t = template.map(str::trim).filter(|t| !t.is_empty()).unwrap_or("{step}_{date}_{time}");
    let at = Local.timestamp_millis_opt(now_ms).single().unwrap_or_else(Local::now);
    let name = t
        .replace("{step}", step)
        .replace("{job}", job)
        .replace("{date}", &at.format("%Y-%m-%d").to_string())
        .replace("{time}", &at.format("%H%M%S").to_string());
    let mut clean: String =
        name.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c }).collect();
    clean = clean.trim().trim_start_matches('.').to_string();
    if clean.is_empty() {
        clean = step.to_string();
    }
    let ext = format!(".{}", format.extension());
    if !clean.to_ascii_lowercase().ends_with(&ext) {
        clean.push_str(&ext);
    }
    clean
}

#[cfg(test)]
mod export_tests {
    use super::*;

    #[test]
    fn file_names() {
        let t = Local.with_ymd_and_hms(2026, 10, 8, 2, 5, 9).unwrap().timestamp_millis();
        assert_eq!(export_file_name(None, "sales", "Nightly", FileFormat::Csv, t), "sales_2026-10-08_020509.csv");
        assert_eq!(export_file_name(Some("{job} {date}"), "s", "Night/ly", FileFormat::Parquet, t), "Night_ly 2026-10-08.parquet");
        assert_eq!(export_file_name(Some("report.JSON"), "s", "j", FileFormat::Json, t), "report.JSON");
        assert_eq!(export_file_name(Some("../x"), "s", "j", FileFormat::Csv, t), "_x.csv");
        assert_eq!(export_file_name(Some("  "), "s", "j", FileFormat::Csv, t), "s_2026-10-08_020509.csv");
    }
}
