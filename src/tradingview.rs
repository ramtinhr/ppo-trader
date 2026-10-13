//! Read-only client for TradingView's chart data feed, used to check our candles, RSI and
//! strategy trades against TradingView's own numbers.
//!
//! The feed is the WebSocket TradingView's chart page uses. Messages are framed as
//! `~m~<length>~m~<json>` and heartbeats (`~h~<n>`) must be echoed back. Running scripts (RSI,
//! strategies) needs a signed-in account; a free one is enough.

use std::{collections::BTreeMap, io::Read, net::TcpStream, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tungstenite::{
    Message, WebSocket, client::IntoClientRequest, http::HeaderValue, stream::MaybeTlsStream,
};

const SOCKET_URL: &str = "wss://data.tradingview.com/socket.io/websocket?from=chart%2F&type=chart";
const PINE_URL: &str = "https://pine-facade.tradingview.com/pine-facade/translate";
const CHART: &str = "cs_dynamic_trader";
const SERIES: &str = "sds_1";

/// Exchanges the browser login cookies (`sessionid`, `sessionid_sign`) for the short-lived
/// token the chart feed accepts.
pub fn auth_token(sessionid: &str, sessionid_sign: &str) -> Result<String> {
    let page = ureq::get("https://www.tradingview.com/")
        .header("Cookie", &format!("sessionid={sessionid}; sessionid_sign={sessionid_sign}"))
        .header("User-Agent", "Mozilla/5.0")
        .call()
        .context("open tradingview.com")?
        .body_mut()
        .with_config()
        .limit(20 * 1024 * 1024)
        .read_to_string()?;
    let start = page
        .find("\"auth_token\":\"")
        .context("TradingView did not accept the login cookies (no auth_token on the page); copy fresh sessionid and sessionid_sign")?
        + "\"auth_token\":\"".len();
    let end = page[start..].find('"').context("unterminated auth_token")? + start;
    anyhow::ensure!(end > start, "TradingView returned an empty auth_token; the login cookies are not valid");
    Ok(page[start..end].to_string())
}

/// A built-in script's compiled body and settings, as needed to add it to a chart.
#[derive(Debug, Clone)]
pub struct PineScript {
    pub id: String,
    pub version: String,
    il: String,
    inputs: Vec<Value>,
}

impl PineScript {
    /// Downloads a built-in script such as `STD;RSI` or `STD;RSI%1Strategy`.
    pub fn fetch(id: &str) -> Result<Self> {
        let url = format!("{PINE_URL}/{}/last", id.replace('%', "%25").replace(';', "%3B"));
        let reply: Value = ureq::get(&url)
            .call()
            .with_context(|| format!("download TradingView script {id}"))?
            .body_mut()
            .read_json()?;
        let result = &reply["result"];
        let il = result["ilTemplate"].as_str().context("script has no ilTemplate")?;
        let meta = &result["metaInfo"];
        Ok(Self {
            id: id.to_string(),
            version: meta["pine"]["version"].as_str().unwrap_or("last").to_string(),
            il: il.to_string(),
            inputs: meta["inputs"].as_array().cloned().unwrap_or_default(),
        })
    }

    /// The `create_study` settings: every input at its default, except `overrides`
    /// (input name, e.g. "RSI Length", to value).
    fn settings(&self, overrides: &[(&str, Value)]) -> Result<Value> {
        let mut settings = serde_json::Map::new();
        settings.insert("text".into(), json!(self.il));
        settings.insert("pineId".into(), json!(self.id));
        settings.insert("pineVersion".into(), json!(self.version));
        for input in &self.inputs {
            let id = input["id"].as_str().unwrap_or_default();
            if !id.starts_with("in_") && id != "pineFeatures" && id != "__profile" {
                continue;
            }
            let name = input["name"].as_str().unwrap_or_default();
            let value = overrides
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| input["defval"].clone());
            settings.insert(
                id.into(),
                json!({ "v": value, "f": true, "t": input["type"] }),
            );
        }
        for (name, _) in overrides {
            anyhow::ensure!(
                self.inputs.iter().any(|i| i["name"] == *name),
                "{} has no input named {name:?}",
                self.id
            );
        }
        Ok(Value::Object(settings))
    }
}

/// One chart bar. `time` is the bar's open time in Unix seconds, as TradingView labels it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TvBar {
    pub time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

/// What a study sent back: its plot values per bar open time, plus the raw strategy report
/// (strategies only).
#[derive(Debug, Default, Clone)]
pub struct StudyOutput {
    pub plots: BTreeMap<i64, Vec<Option<f64>>>,
    pub report: Option<Value>,
}

pub struct ChartSession {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    bars: BTreeMap<i64, TvBar>,
    studies: BTreeMap<String, StudyOutput>,
}

impl ChartSession {
    /// Opens a chart with up to `bar_count` bars of `symbol` (e.g. "OKX:BTCUSDT.P") at
    /// `timeframe` (TradingView resolution in minutes, e.g. "60" for 1 hour). `token` is from
    /// [`auth_token`].
    pub fn open(symbol: &str, timeframe: &str, bar_count: usize, token: &str) -> Result<Self> {
        // Several crates enable both rustls crypto backends, so one must be chosen explicitly.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut request = SOCKET_URL.into_client_request()?;
        request
            .headers_mut()
            .insert("Origin", HeaderValue::from_static("https://www.tradingview.com"));
        let (socket, _) = tungstenite::connect(request).context("connect to TradingView")?;
        if let MaybeTlsStream::Rustls(stream) = socket.get_ref() {
            stream.get_ref().set_read_timeout(Some(Duration::from_secs(60)))?;
        }
        let mut session = Self {
            socket,
            bars: BTreeMap::new(),
            studies: BTreeMap::new(),
        };
        session.send("set_auth_token", json!([token]))?;
        session.send("chart_create_session", json!([CHART, ""]))?;
        let symbol_spec = json!({ "symbol": symbol, "adjustment": "splits" }).to_string();
        session.send("resolve_symbol", json!([CHART, "sds_sym_1", format!("={symbol_spec}")]))?;
        session.send(
            "create_series",
            json!([CHART, SERIES, "s1", "sds_sym_1", timeframe, bar_count, ""]),
        )?;
        session.wait_for("series_completed", None)?;
        Ok(session)
    }

    pub fn bars(&self) -> Vec<TvBar> {
        self.bars.values().copied().collect()
    }

    /// Adds a script to the chart and waits until TradingView has calculated it on every bar.
    pub fn add_study(
        &mut self,
        study_id: &str,
        script: &PineScript,
        overrides: &[(&str, Value)],
    ) -> Result<StudyOutput> {
        let settings = script.settings(overrides)?;
        self.send(
            "create_study",
            json!([CHART, study_id, "st1", SERIES, "Script@tv-scripting-101!", settings]),
        )?;
        self.wait_for("study_completed", Some(study_id))?;
        Ok(self.studies.remove(study_id).unwrap_or_default())
    }

    fn send(&mut self, method: &str, params: Value) -> Result<()> {
        let body = json!({ "m": method, "p": params }).to_string();
        self.socket
            .send(Message::text(format!("~m~{}~m~{body}", body.len())))
            .with_context(|| format!("send {method}"))
    }

    /// Reads messages until `method` arrives (for `study`, if given), storing data on the way.
    fn wait_for(&mut self, method: &str, study: Option<&str>) -> Result<()> {
        loop {
            let message = self.socket.read().context("read from TradingView")?;
            let text = match message {
                Message::Text(text) => text.to_string(),
                Message::Close(frame) => bail!("TradingView closed the connection: {frame:?}"),
                _ => continue,
            };
            for packet in split_packets(&text) {
                if packet.starts_with("~h~") {
                    self.socket
                        .send(Message::text(format!("~m~{}~m~{packet}", packet.len())))?;
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(packet) else {
                    continue;
                };
                let Some(m) = value["m"].as_str() else {
                    continue;
                };
                let p = &value["p"];
                match m {
                    "timescale_update" | "du" => self.store(&p[1])?,
                    "symbol_error" | "series_error" | "study_error" | "critical_error"
                    | "protocol_error" => bail!("TradingView error {m}: {p}"),
                    _ => {}
                }
                if m == method && study.is_none_or(|s| p[1] == s) {
                    return Ok(());
                }
            }
        }
    }

    fn store(&mut self, data: &Value) -> Result<()> {
        let Some(objects) = data.as_object() else {
            return Ok(());
        };
        for (key, object) in objects {
            if key == SERIES {
                for row in object["s"].as_array().into_iter().flatten() {
                    let v = &row["v"];
                    let f = |i: usize| v[i].as_f64().unwrap_or(f64::NAN);
                    let bar = TvBar {
                        time: f(0) as i64,
                        open: f(1),
                        high: f(2),
                        low: f(3),
                        close: f(4),
                    };
                    self.bars.insert(bar.time, bar);
                }
                continue;
            }
            let study = self.studies.entry(key.clone()).or_default();
            for row in object["st"].as_array().into_iter().flatten() {
                let Some(values) = row["v"].as_array() else {
                    continue;
                };
                let time = values.first().and_then(Value::as_f64).unwrap_or(f64::NAN) as i64;
                let plots = values[1..]
                    .iter()
                    .map(|x| x.as_f64().filter(|x| x.abs() < 1e99))
                    .collect();
                study.plots.insert(time, plots);
            }
            if let Some(d) = object["ns"]["d"].as_str().filter(|d| !d.is_empty()) {
                let parsed: Value = serde_json::from_str(d)?;
                if let Some(report) = parsed["data"]["report"].as_object() {
                    study.report = Some(Value::Object(report.clone()));
                } else if let Some(compressed) = parsed["dataCompressed"].as_str() {
                    let unpacked = inflate_base64(compressed)?;
                    if let Some(report) = unpacked["report"].as_object() {
                        study.report = Some(Value::Object(report.clone()));
                    }
                }
            }
        }
        Ok(())
    }
}

fn split_packets(text: &str) -> Vec<&str> {
    let mut packets = Vec::new();
    let mut rest = text;
    while let Some(after) = rest.strip_prefix("~m~") {
        let Some((len, body)) = after.split_once("~m~") else {
            break;
        };
        let Ok(len) = len.parse::<usize>() else {
            break;
        };
        let len = len.min(body.len());
        packets.push(&body[..len]);
        rest = &body[len..];
    }
    packets
}

/// Strategy reports arrive as zlib-compressed JSON in unpadded base64.
fn inflate_base64(data: &str) -> Result<Value> {
    let bytes = base64_decode(data)?;
    let mut text = String::new();
    flate2::read::ZlibDecoder::new(bytes.as_slice())
        .read_to_string(&mut text)
        .context("inflate strategy report")?;
    Ok(serde_json::from_str(&text)?)
}

fn base64_decode(data: &str) -> Result<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in data.bytes().filter(|b| !b.is_ascii_whitespace() && *b != b'=') {
        let value = ALPHABET
            .iter()
            .position(|&a| a == byte)
            .context("invalid base64 in strategy report")? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_framed_packets() {
        let text = "~m~5~m~hello~m~4~m~~h~1";
        assert_eq!(split_packets(text), vec!["hello", "~h~1"]);
    }

    #[test]
    fn decodes_base64() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8").unwrap(), b"hello");
    }

    #[test]
    fn inflates_strategy_report() {
        // zlib-compressed `{"report":1}`, base64 without padding.
        assert_eq!(inflate_base64("eJyrVipKLcgvKlGyMqwFABzVBEQ").unwrap(), json!({"report": 1}));
    }
}
