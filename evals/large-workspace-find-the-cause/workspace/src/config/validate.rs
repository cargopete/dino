use crate::config::schema::Config;
use crate::error::Error;

const MAX_ROUNDING: u64 = 240;

pub fn check(config: &Config) -> Result<(), Error> {
    if config.report.round_to > MAX_ROUNDING {
        return Err(Error::Value {
            key: "report.round_to".to_owned(),
            message: format!("rounding is capped at {MAX_ROUNDING} minutes"),
        });
    }
    if config.report.format.trim().is_empty() {
        return Err(Error::Value {
            key: "report.format".to_owned(),
            message: "must name a plugin".to_owned(),
        });
    }
    for project in config.billing.rates.keys() {
        if project.is_empty() {
            return Err(Error::Value {
                key: "billing.rate.".to_owned(),
                message: "a rate needs a project name".to_owned(),
            });
        }
    }
    Ok(())
}
