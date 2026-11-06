//! 本地真实运动记录模板读取。
//!
//! 此模块只做本地解析和统计，不参与 API 客户端、轨迹上传或 OBS 生成。
//! 适合用真实 GPX/FIT 导出结果校准学校海拔范围；不会把模板重新包装成上传记录。

use serde_json::Value;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct ElevationSample {
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub elevation_m: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TemplateSummary {
    pub source: String,
    pub samples: usize,
    pub min_m: f64,
    pub max_m: f64,
    pub gain_m: f64,
    pub loss_m: f64,
}

/// 模板文件大小上限：GPX/JSON 都是整文件读入，无上限时几个 GB 的文件会直接吃满内存。
const MAX_TEMPLATE_BYTES: u64 = 32 * 1024 * 1024;

pub fn load(path: impl AsRef<Path>) -> Result<Vec<ElevationSample>, String> {
    let path = path.as_ref();
    let meta = fs::metadata(path).map_err(|e| format!("读取模板失败: {e}"))?;
    if !meta.is_file() {
        return Err("模板路径不是普通文件".into());
    }
    if meta.len() > MAX_TEMPLATE_BYTES {
        return Err(format!(
            "模板过大（{:.1} MB），上限 {} MB",
            meta.len() as f64 / 1024.0 / 1024.0,
            MAX_TEMPLATE_BYTES / 1024 / 1024
        ));
    }
    let text = fs::read_to_string(path).map_err(|e| format!("读取模板失败: {e}"))?;
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // 扩展名优先：JSON 里出现 "<trkpt" 这种字符串时，按内容嗅探会误判成 GPX。
    let samples = match ext.as_str() {
        "gpx" => parse_gpx(&text)?,
        "json" => parse_json(&text)?,
        _ if text.contains("<trkpt") => parse_gpx(&text)?,
        _ if text.trim_start().starts_with('{') || text.trim_start().starts_with('[') => {
            parse_json(&text)?
        }
        _ => return Err("仅支持 GPX 或 JSON 本地记录".into()),
    };
    if samples.is_empty() {
        return Err("模板中没有找到海拔采样点".into());
    }
    Ok(samples)
}

/// 纬度合法性：非有限值或越界一律视为「没有坐标」，避免脏数据进入摘要。
fn valid_lat(v: f64) -> Option<f64> {
    (v.is_finite() && (-90.0..=90.0).contains(&v)).then_some(v)
}

/// 经度合法性。
fn valid_lon(v: f64) -> Option<f64> {
    (v.is_finite() && (-180.0..=180.0).contains(&v)).then_some(v)
}

pub fn summarize(path: impl AsRef<Path>, samples: &[ElevationSample]) -> TemplateSummary {
    let mut min_m = f64::INFINITY;
    let mut max_m = f64::NEG_INFINITY;
    let mut gain_m = 0.0;
    let mut loss_m = 0.0;
    for pair in samples.windows(2) {
        let delta = pair[1].elevation_m - pair[0].elevation_m;
        if delta > 0.0 { gain_m += delta; } else { loss_m -= delta; }
    }
    for s in samples {
        min_m = min_m.min(s.elevation_m);
        max_m = max_m.max(s.elevation_m);
    }
    TemplateSummary {
        source: path.as_ref().display().to_string(),
        samples: samples.len(),
        min_m,
        max_m,
        gain_m,
        loss_m,
    }
}

fn parse_gpx(text: &str) -> Result<Vec<ElevationSample>, String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<trkpt") {
        rest = &rest[start..];
        let Some(end) = rest.find('>') else { break };
        let head = &rest[..end];
        let lat = attr(head, "lat");
        let lon = attr(head, "lon");
        let body = &rest[end + 1..];
        let Some(close) = body.find("</trkpt>") else { break };
        let point = &body[..close];
        if let Some(ele) = tag_number(point, "ele") {
            if ele.is_finite() {
                out.push(ElevationSample {
                    latitude: lat.and_then(valid_lat),
                    longitude: lon.and_then(valid_lon),
                    elevation_m: ele,
                });
            }
        }
        rest = &body[close + "</trkpt>".len()..];
    }
    Ok(out)
}

fn attr(head: &str, name: &str) -> Option<f64> {
    let needle = format!("{name}=");
    let pos = head.find(&needle)?;
    let tail = head[pos + needle.len()..].trim_start();
    let quote = tail.chars().next()?;
    let tail = tail.strip_prefix(quote)?;
    let end = tail.find(quote)?;
    tail[..end].parse().ok()
}

fn tag_number(body: &str, name: &str) -> Option<f64> {
    let open = format!("<{name}");
    let start = body.find(&open)?;
    let after = &body[start..];
    let gt = after.find('>')?;
    let text = &after[gt + 1..];
    let end = text.find(&format!("</{name}>"))?;
    text[..end].trim().parse().ok()
}

fn parse_json(text: &str) -> Result<Vec<ElevationSample>, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("JSON 模板格式错误: {e}"))?;
    let mut out = Vec::new();
    collect_json(&value, &mut out);
    Ok(out)
}

fn collect_json(value: &Value, out: &mut Vec<ElevationSample>) {
    match value {
        Value::Array(items) => for item in items { collect_json(item, out); },
        Value::Object(map) => {
            let elevation = ["ele", "elevation", "altitude", "bdA"]
                .iter().find_map(|k| number(map.get(*k)));
            if let Some(elevation_m) = elevation.filter(|v| v.is_finite()) {
                let latitude = ["lat", "latitude", "gLat"]
                    .iter()
                    .find_map(|k| number(map.get(*k)))
                    .and_then(valid_lat);
                let longitude = ["lon", "lng", "longitude", "gLng"]
                    .iter()
                    .find_map(|k| number(map.get(*k)))
                    .and_then(valid_lon);
                out.push(ElevationSample { latitude, longitude, elevation_m });
            }
            for child in map.values() { collect_json(child, out); }
        }
        _ => {}
    }
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gpx_elevation_and_coordinates() {
        let gpx = r#"<gpx><trk><trkseg><trkpt lat="39.9" lon="116.4"><ele>3.3</ele></trkpt><trkpt lat="39.91" lon="116.41"><ele>17.1</ele></trkpt></trkseg></trk></gpx>"#;
        let points = parse_gpx(gpx).unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].elevation_m, 3.3);
        assert_eq!(points[1].latitude, Some(39.91));
    }

    /// 越界/非有限的坐标与海拔不能污染摘要。
    #[test]
    fn rejects_out_of_range_and_non_finite_values() {
        let points = parse_json(
            r#"[{"lat":999,"lon":181,"ele":3.3},{"gLat":39.9,"gLng":116.4,"ele":"NaN"},{"gLat":39.9,"gLng":116.4,"ele":5.0}]"#,
        )
        .unwrap();
        assert_eq!(points.len(), 2, "非有限海拔应被丢弃");
        assert_eq!(points[0].latitude, None);
        assert_eq!(points[0].longitude, None);
        assert!(points[1].latitude.is_some());
    }

    /// 扩展名是 json 时，即使内容里出现 "<trkpt" 也按 JSON 解析。
    #[test]
    fn json_extension_wins_over_content_sniffing() {
        let text = r#"[{"note":"<trkpt lat=\"1\">","ele":7.5}]"#;
        let points = parse_json(text).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].elevation_m, 7.5);
    }

    #[test]
    fn parses_json_aliases() {
        let points = parse_json(r#"[{"gLat":39.9,"gLng":116.4,"bdA":"3.3"},{"latitude":39.91,"longitude":116.41,"elevation":17.1}]"#).unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].elevation_m, 3.3);
    }
}
