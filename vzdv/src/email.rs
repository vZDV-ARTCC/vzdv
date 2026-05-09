//! HTTP endpoints for managing some email configuration.

use crate::{
    config::Config,
    sql::{self, Controller, EmailTemplate},
};
use anyhow::Result;
use lettre::{
    Message, SmtpTransport, Transport, message::header::ContentType,
    transport::smtp::authentication::Credentials,
};
use minijinja::{Environment, context};
use sqlx::{Pool, Sqlite};
use std::collections::HashMap;

/// Email template names.
pub mod templates {
    pub const VISITOR_ACCEPTED: &str = "visitor_accepted";
    pub const VISITOR_DENIED: &str = "visitor_denied";
    pub const VISITOR_REMOVED: &str = "visitor_removed";
    pub const CURRENCY_REQUIRED: &str = "currency_required";
}

/// Email templates by name.
pub struct Templates {
    pub visitor_accepted: EmailTemplate,
    pub visitor_denied: EmailTemplate,
    pub visitor_removed: EmailTemplate,
    pub currency_required: EmailTemplate,
}

/// Send an SMTP email to the recipient.
///
/// Does not do template formatting; for that, use `send_mail`.
pub async fn send_mail_raw(
    config: &Config,
    recipient_address: &str,
    subject: &str,
    body: &str,
) -> Result<()> {
    // construct and send email
    let email = Message::builder()
        .from(config.email.from.parse().unwrap())
        .reply_to(config.email.reply_to.parse().unwrap())
        .to(recipient_address.parse().unwrap())
        .subject(subject.to_owned())
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_owned())
        .unwrap();
    let creds = Credentials::new(
        config.email.user.to_owned(),
        config.email.password.to_owned(),
    );
    let mailer = SmtpTransport::starttls_relay(&config.email.host)
        .unwrap()
        .port(config.email.port)
        .credentials(creds)
        .timeout(Some(std::time::Duration::from_secs(10)))
        .build();
    mailer.send(&email)?;
    Ok(())
}

/// Additional keys that can be provided for email template rendering.
#[derive(Debug, Hash, PartialEq, Eq)]
pub enum EmailExtraKeys {
    QuarterEnd,
    CurrencyHours,
}

/// Send an SMTP email to the recipient.
pub async fn send_mail(
    config: &Config,
    db: &Pool<Sqlite>,
    recipient_name: &str,
    recipient_address: &str,
    template_name: &str,
    extra_info: Option<HashMap<EmailExtraKeys, String>>,
) -> Result<()> {
    let template = query_template(db, template_name).await?;

    // ATM and DATM names for signing
    let atm_datm: Vec<Controller> = sqlx::query_as(sql::GET_ATM_AND_DATM).fetch_all(db).await?;
    let atm = atm_datm
        .iter()
        .find(|controller| controller.roles.contains("ATM") && !controller.roles.contains("DATM"))
        .map(|controller| format!("{} {}, ATM", controller.first_name, controller.last_name))
        .unwrap_or_default();
    let datm = atm_datm
        .iter()
        .find(|controller| controller.roles.contains("DATM"))
        .map(|controller| format!("{} {}, DATM", controller.first_name, controller.last_name))
        .unwrap_or_default();

    // template load and render
    let mut env = Environment::new();
    env.add_template("body", &template.body)?;
    let quarter_end = match &extra_info {
        Some(extra_info) => match extra_info.get(&EmailExtraKeys::QuarterEnd) {
            Some(val) => val.to_owned(),
            None => String::new(),
        },
        None => String::new(),
    };
    let currency_hours = match &extra_info {
        Some(extra_info) => match extra_info.get(&EmailExtraKeys::CurrencyHours) {
            Some(val) => val.to_owned(),
            None => String::new(),
        },
        None => String::new(),
    };
    let body = env
        .get_template("body")?
        .render(context! { recipient_name, atm, datm, quarter_end, currency_hours })?;

    // send the email
    send_mail_raw(config, recipient_address, &template.subject, &body).await?;

    Ok(())
}

/// Get a single template by name.
///
/// Returns an error if the template does not exist.
pub async fn query_template(db: &Pool<Sqlite>, template: &str) -> Result<EmailTemplate> {
    let template = sqlx::query_as(sql::GET_EMAIL_TEMPLATE)
        .bind(template)
        .fetch_one(db)
        .await?;
    Ok(template)
}

/// Load email templates from the database.
pub async fn query_templates(db: &Pool<Sqlite>) -> Result<Templates> {
    let visitor_accepted: EmailTemplate = sqlx::query_as(sql::GET_EMAIL_TEMPLATE)
        .bind(templates::VISITOR_ACCEPTED)
        .fetch_one(db)
        .await?;
    let visitor_denied: EmailTemplate = sqlx::query_as(sql::GET_EMAIL_TEMPLATE)
        .bind(templates::VISITOR_DENIED)
        .fetch_one(db)
        .await?;
    let visitor_removed: EmailTemplate = sqlx::query_as(sql::GET_EMAIL_TEMPLATE)
        .bind(templates::VISITOR_REMOVED)
        .fetch_one(db)
        .await?;
    let currency_required: EmailTemplate = sqlx::query_as(sql::GET_EMAIL_TEMPLATE)
        .bind(templates::CURRENCY_REQUIRED)
        .fetch_one(db)
        .await?;
    Ok(Templates {
        visitor_accepted,
        visitor_denied,
        visitor_removed,
        currency_required,
    })
}
