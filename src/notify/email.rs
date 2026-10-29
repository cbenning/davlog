//! Email alerts over SMTP (lettre, rustls).

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result};
use lettre::message::header::ContentType;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::brand;
use crate::config::{EmailConfig, EmailTls};
use crate::notify::{Event, Notifier, Severity};

const SMTP_TIMEOUT: Duration = Duration::from_secs(30);

pub struct EmailNotifier<T = AsyncSmtpTransport<Tokio1Executor>> {
    transport: T,
    from: Mailbox,
    to: Vec<Mailbox>,
}

impl EmailNotifier {
    pub fn new(cfg: &EmailConfig) -> Result<Self> {
        let builder = match cfg.tls {
            EmailTls::Starttls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.smtp_host)?
            }
            EmailTls::Implicit => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.smtp_host)?,
            EmailTls::None => {
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.smtp_host)
            }
        };
        let transport = builder
            .port(cfg.smtp_port)
            .credentials(Credentials::new(
                cfg.username.clone(),
                cfg.password.expose().to_string(),
            ))
            .timeout(Some(SMTP_TIMEOUT))
            .build();
        Self::with_transport(cfg, transport)
    }
}

impl<T> EmailNotifier<T> {
    pub fn with_transport(cfg: &EmailConfig, transport: T) -> Result<Self> {
        let from = cfg
            .from
            .parse()
            .with_context(|| format!("invalid `from` address {:?}", cfg.from))?;
        let to = cfg
            .to
            .iter()
            .map(|a| {
                a.parse()
                    .with_context(|| format!("invalid `to` address {a:?}"))
            })
            .collect::<Result<Vec<Mailbox>>>()?;
        Ok(EmailNotifier {
            transport,
            from,
            to,
        })
    }

    fn build_message(&self, event: &Event) -> Result<Message> {
        let label = match event.severity {
            Severity::Warning => "warning",
            Severity::Error => "error",
            Severity::Recovered => "recovered",
        };
        let at = event
            .at
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut builder = Message::builder()
            .from(self.from.clone())
            .subject(format!("[{}] {label}: {}", brand::NAME, event.summary))
            .header(ContentType::TEXT_PLAIN);
        for to in &self.to {
            builder = builder.to(to.clone());
        }
        Ok(builder.body(format!(
            "{}\n\n{}\n\nTime (unix): {at}\n",
            event.summary, event.details
        ))?)
    }
}

impl<T> Notifier for EmailNotifier<T>
where
    T: AsyncTransport + Send + Sync + 'static,
    T::Error: std::error::Error + Send + Sync + 'static,
{
    fn name(&self) -> &str {
        "email"
    }

    fn send<'a>(
        &'a self,
        event: &'a Event,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let message = self.build_message(event)?;
            self.transport.send(message).await.context("send email")?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EmailConfig, EmailTls, Secret};
    use crate::notify::{Event, EventKind, Notifier, Severity};
    use lettre::transport::stub::AsyncStubTransport;

    fn cfg(tls: EmailTls) -> EmailConfig {
        EmailConfig {
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            tls,
            username: "alerts@example.com".into(),
            password: Secret::new("secret".into()),
            from: "davgit <alerts@example.com>".into(),
            to: vec!["me@example.com".into(), "you@example.com".into()],
        }
    }

    fn event() -> Event {
        Event::new(
            EventKind::PushPaused,
            Severity::Error,
            "push paused",
            "ahead 1, behind 2\nrun: git -C /srv/notes fetch origin",
        )
    }

    #[tokio::test]
    async fn email_message_has_from_to_subject_and_body() {
        let transport = AsyncStubTransport::new_ok();
        let notifier =
            EmailNotifier::with_transport(&cfg(EmailTls::Starttls), transport.clone()).unwrap();
        notifier.send(&event()).await.unwrap();

        let sent = transport.messages().await;
        assert_eq!(sent.len(), 1);
        let (envelope, raw) = &sent[0];
        let recipients: Vec<String> = envelope.to().iter().map(|a| a.to_string()).collect();
        assert_eq!(recipients, vec!["me@example.com", "you@example.com"]);
        assert!(
            raw.contains("Subject: [davgit] error: push paused"),
            "{raw}"
        );
        assert!(raw.contains("alerts@example.com"), "{raw}");
        assert!(raw.contains("ahead 1, behind 2"), "{raw}");
        assert!(raw.contains("git -C /srv/notes fetch origin"), "{raw}");
        assert!(
            !raw.contains("secret"),
            "password must not appear in the message"
        );
    }

    #[tokio::test]
    async fn transport_errors_are_returned_not_panicked() {
        let transport = AsyncStubTransport::new_error();
        let notifier = EmailNotifier::with_transport(&cfg(EmailTls::Starttls), transport).unwrap();
        assert!(notifier.send(&event()).await.is_err());
    }

    #[test]
    fn email_tls_modes_map_to_transport_builders() {
        for tls in [EmailTls::Starttls, EmailTls::Implicit, EmailTls::None] {
            EmailNotifier::new(&cfg(tls)).unwrap_or_else(|e| panic!("{tls:?}: {e:#}"));
        }
    }

    #[test]
    fn invalid_addresses_are_rejected_at_construction() {
        let mut bad_from = cfg(EmailTls::Starttls);
        bad_from.from = "not an address".into();
        assert!(EmailNotifier::new(&bad_from).is_err());
        let mut bad_to = cfg(EmailTls::Starttls);
        bad_to.to = vec!["nope".into()];
        assert!(EmailNotifier::new(&bad_to).is_err());
    }
}
