use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub risk: RiskConfig,
    pub venues: VenuesConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskConfig {
    pub paper_only: bool,
    pub max_leverage: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenuesConfig {
    pub okx: VenueConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenueConfig {
    pub maker_fee_bps: f64,
    pub taker_fee_bps: f64,
}

impl AppConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read configuration {}", path.display()))?;
        let config: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse configuration {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if !self.risk.paper_only {
            bail!("live trading is disabled; risk.paper_only must be true");
        }
        if !(1..=10).contains(&self.risk.max_leverage) {
            bail!("initial max_leverage must be between 1 and 10");
        }
        let okx = &self.venues.okx;
        if okx.maker_fee_bps < 0.0 || okx.taker_fee_bps < 0.0 {
            bail!("okx fees cannot be negative");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_CONFIG: &str = r#"
[risk]
paper_only = true
max_leverage = 2

[venues.okx]
maker_fee_bps = 2.0
taker_fee_bps = 5.0
"#;

    #[test]
    fn parses_and_validates() {
        let config: AppConfig = toml::from_str(VALID_CONFIG).unwrap();
        config.validate().unwrap();
        assert_eq!(config.venues.okx.taker_fee_bps, 5.0);
    }

    #[test]
    fn rejects_unsafe_leverage_and_live_trading() {
        let mut config: AppConfig = toml::from_str(VALID_CONFIG).unwrap();
        config.risk.max_leverage = 10;
        assert!(config.validate().is_ok());
        config.risk.max_leverage = 11;
        assert!(config.validate().is_err());
        let mut config: AppConfig = toml::from_str(VALID_CONFIG).unwrap();
        config.risk.paper_only = false;
        assert!(config.validate().is_err());
    }
}
