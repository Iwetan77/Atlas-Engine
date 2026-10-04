//! Emails about the user's money, from "Ebube from Atlas" through Senviok: money in (bank top-ups,
//! deposits, a friend paying them), money out (bank cash-outs, sends, links, withdrawals), trades and
//! predictions, and perps alerts (liquidations, a take-profit or stop-loss closing a position, a
//! position close to liquidation). Each is sent once (a key per event), only to the sign-in email,
//! and never when the user has turned them off in Profile. Without SENVIOK_API_KEY nothing is sent.
use super::*;
use engine_execution::hyperliquid::DEXES;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

const API: &str = "https://api.senviok.live/v1/emails";
const SITE: &str = "https://justatlas.xyz";
const PINK: &str = "#FF2E7E";
// The perps watcher: how often it looks, and how far along to liquidation counts as close.
const WATCH_EVERY: Duration = Duration::from_secs(60);
const NEAR_LIQUIDATION: f64 = 0.3;
const HALF_DAY_MS: u64 = 12 * 60 * 60 * 1000;

#[derive(Clone, Debug, Default, PartialEq)]
struct Contact {
    email: String,
    name: Option<String>,
    enabled: bool,
    currency: String,
    wallet: Option<String>,
    perps_watch: bool,
    perps_seen_ms: u64,
}

#[derive(Default)]
struct Memory {
    contacts: HashMap<String, Contact>,
    sent: HashSet<String>,
}

#[derive(Clone)]
pub(super) struct EmailState {
    key: Option<String>,
    from: String,
    from_name: String,
    http: reqwest::Client,
    postgres: Option<Arc<tokio_postgres::Client>>,
    memory: Arc<Mutex<Memory>>,
    // What's already stored for each user, so a busy Home doesn't write on every refresh.
    remembered: Arc<Mutex<HashMap<String, String>>>,
    // Statuses already looked at by this process (the database key still decides what's sent).
    handled: Arc<Mutex<HashSet<String>>>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    eprintln!("emails: {error}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Email settings are unavailable. Try again shortly.".into(),
    )
}

fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub(super) fn configured() -> bool {
    clean(env::var("SENVIOK_API_KEY").ok()).is_some()
}

impl EmailState {
    pub(super) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let postgres = if let Ok(url) = env::var("DATABASE_URL") {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    eprintln!("emails database connection ended: {error}");
                }
            });
            client
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS atlas_email_contacts (
                    user_id TEXT PRIMARY KEY,
                    email TEXT NOT NULL,
                    name TEXT,
                    enabled BOOLEAN NOT NULL DEFAULT TRUE,
                    currency TEXT NOT NULL DEFAULT 'NGN',
                    wallet TEXT,
                    perps_watch BOOLEAN NOT NULL DEFAULT FALSE,
                    perps_seen_ms BIGINT NOT NULL DEFAULT 0
                );
                CREATE TABLE IF NOT EXISTS atlas_emails_sent (
                    key TEXT PRIMARY KEY,
                    sent_ms BIGINT NOT NULL
                )",
                )
                .await?;
            Some(Arc::new(client))
        } else {
            None
        };
        Ok(Self {
            key: clean(env::var("SENVIOK_API_KEY").ok()),
            from: clean(env::var("ATLAS_EMAIL_FROM").ok())
                .unwrap_or_else(|| "hello@justatlas.xyz".into()),
            from_name: clean(env::var("ATLAS_EMAIL_FROM_NAME").ok())
                .unwrap_or_else(|| "Ebube from Atlas".into()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?,
            postgres,
            memory: Arc::new(Mutex::new(Memory::default())),
            remembered: Arc::new(Mutex::new(HashMap::new())),
            handled: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    // Keeps the user's sign-in email, name, currency and wallet, so emails can reach them later
    // (a bank top-up landing, a friend paying them, a liquidation) without them in the app.
    pub(super) async fn remember(
        &self,
        user: &app_balance::VerifiedWallets,
        currency: Option<&str>,
    ) -> Result<(), ApiError> {
        let Some(email) = clean(user.email.clone()).filter(|e| e.contains('@')) else {
            return Ok(());
        };
        let name = clean(user.name.clone());
        let wallet = clean(user.evm_wallet.clone());
        let print = format!("{email}|{name:?}|{currency:?}|{wallet:?}");
        if self.remembered.lock().map_err(internal)?.get(&user.user_id) == Some(&print) {
            return Ok(());
        }
        if let Some(pg) = &self.postgres {
            pg.execute(
                "INSERT INTO atlas_email_contacts (user_id, email, name, currency, wallet)
                 VALUES ($1, $2, $3, COALESCE($4, 'NGN'), $5)
                 ON CONFLICT (user_id) DO UPDATE SET email=$2, name=$3,
                   currency=COALESCE($4, atlas_email_contacts.currency), wallet=$5",
                &[&user.user_id, &email, &name, &currency, &wallet],
            )
            .await
            .map_err(internal)?;
        } else {
            let mut memory = self.memory.lock().map_err(internal)?;
            let contact = memory
                .contacts
                .entry(user.user_id.clone())
                .or_insert_with(|| Contact {
                    enabled: true,
                    currency: "NGN".into(),
                    ..Contact::default()
                });
            contact.email = email;
            contact.name = name;
            contact.wallet = wallet;
            if let Some(c) = currency {
                contact.currency = c.into();
            }
        }
        self.remembered
            .lock()
            .map_err(internal)?
            .insert(user.user_id.clone(), print);
        Ok(())
    }

    async fn contact(&self, user_id: &str) -> Result<Option<Contact>, ApiError> {
        if let Some(pg) = &self.postgres {
            let row = pg
                .query_opt(
                    "SELECT email, name, enabled, currency, wallet, perps_watch, perps_seen_ms
                     FROM atlas_email_contacts WHERE user_id=$1",
                    &[&user_id],
                )
                .await
                .map_err(internal)?;
            return Ok(row.map(|r| contact_from_row(&r)));
        }
        Ok(self
            .memory
            .lock()
            .map_err(internal)?
            .contacts
            .get(user_id)
            .cloned())
    }

    async fn set_enabled(&self, user_id: &str, enabled: bool) -> Result<bool, ApiError> {
        if let Some(pg) = &self.postgres {
            let changed = pg
                .execute(
                    "UPDATE atlas_email_contacts SET enabled=$2 WHERE user_id=$1",
                    &[&user_id, &enabled],
                )
                .await
                .map_err(internal)?;
            return Ok(changed == 1);
        }
        let mut memory = self.memory.lock().map_err(internal)?;
        Ok(memory
            .contacts
            .get_mut(user_id)
            .map(|c| c.enabled = enabled)
            .is_some())
    }

    // Starts watching a perps account, from now: fills before this aren't news.
    pub(super) async fn watch_perps(&self, user_id: &str) -> Result<(), ApiError> {
        if let Some(pg) = &self.postgres {
            pg.execute(
                "UPDATE atlas_email_contacts SET perps_watch=TRUE, perps_seen_ms=$2
                 WHERE user_id=$1 AND NOT perps_watch",
                &[&user_id, &(now() as i64)],
            )
            .await
            .map_err(internal)?;
            return Ok(());
        }
        if let Some(c) = self
            .memory
            .lock()
            .map_err(internal)?
            .contacts
            .get_mut(user_id)
            .filter(|c| !c.perps_watch)
        {
            c.perps_watch = true;
            c.perps_seen_ms = now();
        }
        Ok(())
    }

    async fn watched(&self) -> Result<Vec<(String, Contact)>, ApiError> {
        if let Some(pg) = &self.postgres {
            return Ok(pg
                .query(
                    "SELECT user_id, email, name, enabled, currency, wallet, perps_watch, perps_seen_ms
                     FROM atlas_email_contacts WHERE perps_watch AND enabled AND wallet IS NOT NULL
                     ORDER BY perps_seen_ms LIMIT 200",
                    &[],
                )
                .await
                .map_err(internal)?
                .iter()
                .map(|r| (r.get::<_, String>("user_id"), contact_from_row(r)))
                .collect());
        }
        Ok(self
            .memory
            .lock()
            .map_err(internal)?
            .contacts
            .iter()
            .filter(|(_, c)| c.perps_watch && c.enabled && c.wallet.is_some())
            .map(|(id, c)| (id.clone(), c.clone()))
            .collect())
    }

    async fn seen_perps(&self, user_id: &str, seen_ms: u64, watch: bool) -> Result<(), ApiError> {
        if let Some(pg) = &self.postgres {
            pg.execute(
                "UPDATE atlas_email_contacts SET perps_seen_ms=GREATEST(perps_seen_ms,$2),
                 perps_watch=$3 WHERE user_id=$1",
                &[&user_id, &(seen_ms as i64), &watch],
            )
            .await
            .map_err(internal)?;
            return Ok(());
        }
        if let Some(c) = self
            .memory
            .lock()
            .map_err(internal)?
            .contacts
            .get_mut(user_id)
        {
            c.perps_seen_ms = c.perps_seen_ms.max(seen_ms);
            c.perps_watch = watch;
        }
        Ok(())
    }

    // Takes the one send an event gets; false when it has already gone (or is going).
    async fn claim(&self, key: &str) -> Result<bool, ApiError> {
        if let Some(pg) = &self.postgres {
            let added = pg
                .execute(
                    "INSERT INTO atlas_emails_sent (key, sent_ms) VALUES ($1, $2)
                     ON CONFLICT (key) DO NOTHING",
                    &[&key, &(now() as i64)],
                )
                .await
                .map_err(internal)?;
            return Ok(added == 1);
        }
        Ok(self
            .memory
            .lock()
            .map_err(internal)?
            .sent
            .insert(key.into()))
    }

    async fn release(&self, key: &str) {
        if let Some(pg) = &self.postgres {
            let _ = pg
                .execute("DELETE FROM atlas_emails_sent WHERE key=$1", &[&key])
                .await;
        } else if let Ok(mut memory) = self.memory.lock() {
            memory.sent.remove(key);
        }
    }

    // True the first time this process sees `key`.
    fn first_look(&self, key: String) -> bool {
        let Ok(mut handled) = self.handled.lock() else {
            return false;
        };
        if handled.len() > 20_000 {
            handled.clear();
        }
        handled.insert(key)
    }

    async fn post(&self, to: &str, email: &Email) -> Result<(), String> {
        let key = self.key.as_deref().ok_or("SENVIOK_API_KEY is not set")?;
        let body = json!({"from":self.from,"fromName":self.from_name,"to":to,
            "subject":email.subject,"html":email.html,"text":email.text,"replyTo":self.from});
        for attempt in 0..2 {
            let response = self
                .http
                .post(API)
                .bearer_auth(key)
                .json(&body)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = response.status();
            if status.is_success() {
                return Ok(());
            }
            // Over Senviok's pace (100 a minute): wait as asked, once.
            if status == StatusCode::TOO_MANY_REQUESTS && attempt == 0 {
                let wait = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(5)
                    .min(30);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            let text = response.text().await.unwrap_or_default();
            return Err(format!(
                "Senviok answered {status}: {}",
                text.chars().take(200).collect::<String>()
            ));
        }
        Err("Senviok is busy".into())
    }

    // Sends `email` to `user_id` once per `key`, if they have an address and want these.
    async fn deliver(
        &self,
        user_id: &str,
        key: &str,
        email: impl FnOnce(&Contact) -> Option<Email>,
    ) {
        if self.key.is_none() {
            return;
        }
        let contact = match self.contact(user_id).await {
            Ok(Some(c)) if c.enabled && !c.email.is_empty() => c,
            _ => return,
        };
        let Some(email) = email(&contact) else {
            return;
        };
        match self.claim(key).await {
            Ok(true) => {}
            _ => return,
        }
        if let Err(reason) = self.post(&contact.email, &email).await {
            eprintln!("email {key} not sent: {reason}");
            self.release(key).await;
        }
    }
}

fn contact_from_row(r: &tokio_postgres::Row) -> Contact {
    Contact {
        email: r.get("email"),
        name: r.get("name"),
        enabled: r.get("enabled"),
        currency: r.get("currency"),
        wallet: r.get("wallet"),
        perps_watch: r.get("perps_watch"),
        perps_seen_ms: u64::try_from(r.get::<_, i64>("perps_seen_ms")).unwrap_or(0),
    }
}

// ---- The email itself ----

#[derive(Debug, PartialEq)]
struct Email {
    subject: String,
    html: String,
    text: String,
}

// What one email says: the subject, the sentence after the greeting, and a card with the details.
#[derive(Debug, Default, PartialEq)]
struct Message {
    subject: String,
    lead: String,
    heading: String,
    amount: Option<String>,
    lines: Vec<(String, String)>,
    // Red for bad news (a liquidation, a payout that failed), pink otherwise.
    warning: bool,
    link: String,
}

fn esc(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

// "Ebuka" from "Ebuka Okafor"; "there" without a name.
fn first_name(name: Option<&str>) -> String {
    name.and_then(|n| n.split_whitespace().next())
        .filter(|n| n.chars().any(char::is_alphabetic))
        .map(|n| {
            let mut chars = n.chars();
            chars.next().map_or_else(String::new, |c| {
                c.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
            })
        })
        .unwrap_or_else(|| "there".into())
}

fn render(name: Option<&str>, m: &Message) -> Email {
    let hi = first_name(name);
    let accent = if m.warning { "#E5484D" } else { PINK };
    let rows: String = m
        .lines
        .iter()
        .take(7)
        .map(|(label, value)| {
            format!(
                r#"<tr><td style="padding:8px 0;color:#64748B;font-size:14px;">{}</td><td style="padding:8px 0;color:#0F172A;font-size:14px;font-weight:600;text-align:right;">{}</td></tr>"#,
                esc(label),
                esc(value)
            )
        })
        .collect();
    let amount = m.amount.as_deref().map_or(String::new(), |a| {
        format!(
            r#"<div style="font-size:30px;line-height:36px;font-weight:700;color:#0F172A;margin:4px 0 12px;">{}</div>"#,
            esc(a)
        )
    });
    let html = format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{subject}</title></head>
<body style="margin:0;padding:0;background:#F4F5F7;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="background:#F4F5F7;padding:24px 12px;"><tr><td align="center">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="max-width:520px;">
<tr><td style="padding:4px 4px 20px;"><img src="{site}/atlas-icon.png" width="36" height="36" alt="Atlas" style="display:block;border-radius:10px;"></td></tr>
<tr><td style="background:#FFFFFF;border-radius:20px;padding:28px 24px;">
<p style="margin:0 0 14px;font-size:16px;line-height:24px;color:#0F172A;">Hi {hi},</p>
<p style="margin:0 0 22px;font-size:16px;line-height:24px;color:#0F172A;">It's Ebube from Atlas. {lead}</p>
<div style="border-radius:16px;background:#F8F9FB;padding:18px 18px 10px;border-top:4px solid {accent};">
<div style="font-size:13px;letter-spacing:.02em;text-transform:uppercase;color:{accent};font-weight:700;">{heading}</div>
{amount}<table role="presentation" width="100%" cellpadding="0" cellspacing="0">{rows}</table></div>
<table role="presentation" cellpadding="0" cellspacing="0" style="margin:24px 0 6px;"><tr><td style="border-radius:999px;background:{pink};">
<a href="{link}" style="display:inline-block;padding:13px 26px;font-size:15px;font-weight:600;color:#FFFFFF;text-decoration:none;border-radius:999px;">Open Atlas</a></td></tr></table>
<p style="margin:22px 0 0;font-size:15px;line-height:22px;color:#0F172A;">Ebube<br><span style="color:#64748B;">Atlas</span></p>
</td></tr>
<tr><td style="padding:18px 8px;font-size:12px;line-height:18px;color:#94A3B8;text-align:center;">You're getting this because you have an Atlas account. Turn these emails off any time in Atlas &rarr; Profile &rarr; Email me about my money.<br><a href="{site}" style="color:#94A3B8;">justatlas.xyz</a></td></tr>
</table></td></tr></table></body></html>"#,
        subject = esc(&m.subject),
        site = SITE,
        hi = esc(&hi),
        lead = esc(&m.lead),
        accent = accent,
        heading = esc(&m.heading),
        amount = amount,
        rows = rows,
        pink = PINK,
        link = esc(&m.link),
    );
    let mut text = format!(
        "Hi {hi},\n\nIt's Ebube from Atlas. {}\n\n{}",
        m.lead, m.heading
    );
    if let Some(a) = &m.amount {
        text += &format!(": {a}");
    }
    text += "\n";
    for (label, value) in m.lines.iter().take(7) {
        text += &format!("{label}: {value}\n");
    }
    text += &format!(
        "\nOpen Atlas: {}\n\nEbube\nAtlas\n\nYou can turn these emails off in Atlas > Profile > Email me about my money.\n",
        m.link
    );
    Email {
        subject: m.subject.clone(),
        html,
        text,
    }
}

// Lines worth repeating in an email (not the internals a receipt sometimes carries).
fn card_lines(t: &transactions::Told, skip: &[&str]) -> Vec<(String, String)> {
    t.lines
        .iter()
        .filter(|(l, v)| !skip.contains(&l.as_str()) && v.chars().count() <= 80)
        .cloned()
        .collect()
}

// What a finished receipt says by email, or nothing when it isn't news (a failure the user saw
// happen in the app, a bank cash-out whose payout email comes later, a top-up nobody paid).
fn receipt_message(t: &transactions::Told, amount: Option<String>) -> Option<Message> {
    let filled = t.state == "filled";
    let failed = t.state == "failed";
    let link = format!("{SITE}/transaction/{}", t.id);
    let symbol = if t.symbol.is_empty() { "it" } else { &t.symbol };
    let say = amount.clone().unwrap_or_else(|| "Your money".into());
    let msg =
        |subject: String, lead: String, heading: &str, lines: Vec<(String, String)>| Message {
            subject,
            lead,
            heading: heading.into(),
            amount: amount.clone(),
            lines,
            warning: failed,
            link: link.clone(),
        };
    if t.id.starts_with("prediction-") {
        if !filled {
            return None;
        }
        let market = t.line("Market").unwrap_or("your market").to_string();
        let lines = card_lines(t, &[]);
        return Some(match t.kind.as_str() {
            "buy" => msg(
                "Your prediction is in".into(),
                format!(
                    "You're in on \"{market}\" ({}).",
                    t.line("Your choice").unwrap_or("your pick")
                ),
                "Prediction",
                lines,
            ),
            "sell" => msg(
                "You sold your prediction".into(),
                format!("You sold your shares in \"{market}\". The cash is in your balance."),
                "Prediction sold",
                lines,
            ),
            _ => msg(
                "Your prediction cash is back".into(),
                "Cash from Atlas Predictions is back in your balance.".into(),
                "Predictions",
                lines,
            ),
        });
    }
    match (t.kind.as_str(), filled, failed) {
        ("onramp", true, _) => Some(msg(
            format!("{say} added to your Atlas balance"),
            format!("Your bank transfer arrived, and {} is now in your balance.", say.to_lowercase_first()),
            "Money added",
            card_lines(t, &["Status"]),
        )),
        ("onramp", _, true) if t.line("Status") != Some("Expired") => Some(msg(
            "Your top-up needs attention".into(),
            t.error.clone().unwrap_or_else(|| "Your bank transfer didn't go through. Reply to this email and we'll sort it out.".into()),
            "Top-up",
            card_lines(t, &["Status"]),
        )),
        ("deposit", true, _) => Some(msg(
            format!("Your {symbol} deposit arrived"),
            format!("Your {symbol} deposit landed and is now in your balance."),
            "Deposit",
            Vec::new(),
        )),
        ("deposit", _, true) => Some(msg(
            "Your deposit didn't go through".into(),
            t.error.clone().unwrap_or_else(|| "Your deposit couldn't be completed.".into()),
            "Deposit",
            Vec::new(),
        )),
        ("offramp", _, _) => {
            let payout = t.line("Bank payout").unwrap_or_default();
            if let Some(paid) = payout.strip_prefix("Paid ") {
                Some(Message {
                    amount: Some(paid.into()),
                    ..msg(
                        format!("{paid} sent to your bank"),
                        format!("Your cash out is done: {paid} has been paid to your bank."),
                        "Cash out",
                        card_lines(t, &["Bank payout", "Rate", "You pay"]),
                    )
                })
            } else if payout.starts_with("Failed") || (t.error.is_some() && failed) {
                Some(Message {
                    warning: true,
                    ..msg(
                        "Your cash out needs attention".into(),
                        "Your bank payout didn't go through. Reply to this email and we'll sort it out."
                            .into(),
                        "Cash out",
                        card_lines(t, &["Rate"]),
                    )
                })
            } else {
                None
            }
        }
        ("buy", true, _) => Some(msg(
            format!("You bought {symbol}"),
            format!("Your {symbol} buy went through."),
            "Buy",
            card_lines(t, &["Market"]),
        )),
        ("sell", true, _) => Some(msg(
            format!("You sold {symbol}"),
            format!("You sold {symbol}. The cash is in your balance."),
            "Sell",
            card_lines(t, &["Market"]),
        )),
        ("withdraw", true, _) => Some(msg(
            format!("Your {symbol} withdrawal went through"),
            format!("Your {symbol} is on its way to your wallet."),
            "Withdraw",
            card_lines(t, &[]),
        )),
        ("earn_deposit", true, _) => Some(msg(
            format!("You're saving with {symbol}"),
            format!("{say} is now saving with {symbol}."),
            "Savings",
            card_lines(t, &[]),
        )),
        ("earn_withdraw", true, _) => Some(msg(
            format!("Your savings are back from {symbol}"),
            format!("{say} from {symbol} is back in your balance."),
            "Savings",
            card_lines(t, &[]),
        )),
        ("perp_open", true, _) => {
            let action = t.line("Action").unwrap_or("position").to_lowercase();
            Some(msg(
                format!("Your {symbol} position is open"),
                format!("Your {action} on {symbol} is open."),
                "Perps",
                card_lines(t, &["Market"]),
            ))
        }
        ("perp_close", true, _) => Some(msg(
            format!("You closed your {symbol} position"),
            format!("Your {symbol} position is closed, and the cash is back in your balance."),
            "Perps",
            card_lines(t, &["Market", "Action"]),
        )),
        ("cashlink", true, _) => Some(msg(
            "Your Atlas Link is ready".into(),
            "Your Atlas Link is live. Anyone you share it with can claim it.".into(),
            "Atlas Link",
            card_lines(t, &[]),
        )),
        ("send", true, _) => {
            let to = t.line("Send to").unwrap_or("them").to_string();
            Some(msg(
                format!("You sent {say} to {to}"),
                if to.starts_with('@') {
                    format!("You sent {} to {to}. It's already in their Atlas balance.", say.to_lowercase_first())
                } else {
                    format!("You sent {} to {to}.", say.to_lowercase_first())
                },
                "Sent",
                card_lines(t, &["Send to"]),
            ))
        }
        _ => None,
    }
}

trait LowerFirst {
    fn to_lowercase_first(&self) -> String;
}
impl LowerFirst for String {
    // "Your money" mid-sentence reads "your money"; an amount stays as it is.
    fn to_lowercase_first(&self) -> String {
        if self == "Your money" {
            "your money".into()
        } else {
            self.clone()
        }
    }
}

// ---- When to send ----

// The receipt `id` of `owner` has news: email them about it (once per `key`).
async fn about_receipt(state: &AppState, owner: &str, id: &str, key: &str) {
    let Ok(Some(contact)) = state.emails.contact(owner).await else {
        return;
    };
    if !contact.enabled {
        return;
    }
    let told = match transactions::told(state, owner, id, &contact.currency).await {
        Ok(Some(t)) => t,
        _ => return,
    };
    let rate = app_balance::fx_rate(&contact.currency).await.ok();
    let amount = told
        .units
        .zip(rate)
        .map(|(u, r)| markets::say_money(u, &contact.currency, r));
    let Some(message) = receipt_message(&told, amount) else {
        return;
    };
    state
        .emails
        .deliver(owner, key, |c| Some(render(c.name.as_deref(), &message)))
        .await;
    // A friend paid from their balance: the friend hears about it too.
    if told.kind == "send" && told.state == "filled" {
        if let Some(handle) = told.line("Send to").and_then(|to| to.strip_prefix('@')) {
            to_recipient(state, owner, handle, &told, key).await;
        }
    }
}

async fn to_recipient(
    state: &AppState,
    sender: &str,
    handle: &str,
    told: &transactions::Told,
    key: &str,
) {
    let Ok(Some(recipient)) = state.social.user_of_handle(handle).await else {
        return;
    };
    if recipient == sender {
        return;
    }
    let from = match state.social.handle_of(sender).await {
        Ok(Some(h)) => format!("@{h}"),
        _ => "A friend".into(),
    };
    let Some(units) = told.units else {
        return;
    };
    let Ok(Some(contact)) = state.emails.contact(&recipient).await else {
        return;
    };
    let Ok(rate) = app_balance::fx_rate(&contact.currency).await else {
        return;
    };
    let amount = markets::say_money(units, &contact.currency, rate);
    let message = Message {
        subject: format!("{from} sent you {amount}"),
        lead: format!("{from} just sent you {amount}. It's already in your Atlas balance."),
        heading: "Money in".into(),
        amount: Some(amount),
        lines: vec![("From".into(), from.clone())],
        warning: false,
        link: SITE.into(),
    };
    state
        .emails
        .deliver(&recipient, &format!("{key}:to"), |c| {
            Some(render(c.name.as_deref(), &message))
        })
        .await;
}

// After any status the app reads: a finished one may be news.
pub(super) fn after_status(state: &AppState, headers: &HeaderMap, status: &markets::IntentStatus) {
    if state.emails.key.is_none() || !matches!(status.state.as_str(), "filled" | "failed") {
        return;
    }
    let key = format!("tx:{}:{}", status.intent_id, status.state);
    if !state.emails.first_look(key.clone()) {
        return;
    }
    let (state, headers, id) = (state.clone(), headers.clone(), status.intent_id.clone());
    tokio::spawn(async move {
        let Ok(user) = app_balance::verified_wallets(&state, &headers).await else {
            return;
        };
        let _ = state.emails.remember(&user, None).await;
        about_receipt(&state, &user.user_id, &id, &key).await;
    });
}

// A bank top-up or cash-out moved on (Daya's webhook or a check).
pub(super) fn ramp_changed(state: &AppState, owner: &str, receipt: &str, kind: &str, status: &str) {
    let news = matches!(status, "completed" | "failed");
    if state.emails.key.is_none() || !news || !matches!(kind, "onramp" | "offramp") {
        return;
    }
    let key = format!("ramp:{receipt}:{status}");
    if !state.emails.first_look(key.clone()) {
        return;
    }
    let (state, owner, receipt) = (state.clone(), owner.to_string(), receipt.to_string());
    tokio::spawn(async move { about_receipt(&state, &owner, &receipt, &key).await });
}

// A deposit from another network settled (or was refunded).
pub(super) fn deposit_changed(state: &AppState, owner: &str, address: &str, status: &str) {
    if state.emails.key.is_none() || !matches!(status, "SUCCESS" | "REFUNDED" | "FAILED") {
        return;
    }
    let key = format!("deposit:{address}:{status}");
    if !state.emails.first_look(key.clone()) {
        return;
    }
    let (state, owner, id) = (
        state.clone(),
        owner.to_string(),
        format!("deposit-{address}"),
    );
    tokio::spawn(async move { about_receipt(&state, &owner, &id, &key).await });
}

// ---- Routes ----

// The app's status poll, unchanged, with emails for what finished.
pub(super) async fn intent_status(
    State(state): State<AppState>,
    Path(intent_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    let answer =
        markets::intent_status(State(state.clone()), Path(intent_id), headers.clone()).await?;
    after_status(&state, &headers, &answer.0);
    Ok(answer)
}

pub(super) async fn signed(
    State(state): State<AppState>,
    Path(intent_id): Path<String>,
    headers: HeaderMap,
    body: Json<markets::Submission>,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    pin::require_intent(&state, &headers, &intent_id).await?;
    let answer =
        markets::signed(State(state.clone()), Path(intent_id), headers.clone(), body).await?;
    after_status(&state, &headers, &answer.0);
    Ok(answer)
}

#[derive(Deserialize)]
pub(super) struct Settings {
    enabled: bool,
}

// Profile's "Email me about my money": where they go, and whether they're on.
pub(super) async fn settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    state.emails.remember(&user, None).await?;
    let contact = state.emails.contact(&user.user_id).await?;
    Ok(Json(json!({"available":state.emails.key.is_some(),
        "email":contact.as_ref().map(|c| c.email.clone()),
        "enabled":contact.is_some_and(|c| c.enabled)})))
}

pub(super) async fn update_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Settings>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    state.emails.remember(&user, None).await?;
    if !state
        .emails
        .set_enabled(&user.user_id, body.enabled)
        .await?
    {
        return Err((
            StatusCode::CONFLICT,
            "Your account has no email address to send to.".into(),
        ));
    }
    settings(State(state), headers).await
}

// ---- Perps alerts ----

// Every minute: for each watched perps account, a liquidation, a take-profit or stop-loss that
// closed a position, or a position close to liquidation. Accounts with nothing left open stop being
// watched until the next trade.
pub(super) fn keep_watching(state: AppState) {
    if state.emails.key.is_none() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(WATCH_EVERY).await;
            let Ok(accounts) = state.emails.watched().await else {
                continue;
            };
            for (user_id, contact) in accounts {
                if let Err(reason) = watch_one(&state, &user_id, &contact).await {
                    eprintln!("perps alerts for {user_id} skipped: {reason}");
                }
                // Hyperliquid's info API is shared: pace it.
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        }
    });
}

async fn watch_one(state: &AppState, user_id: &str, contact: &Contact) -> Result<(), String> {
    let wallet = contact.wallet.as_deref().ok_or("no wallet")?;
    let client = hl::client(state);
    let rate = app_balance::fx_rate(&contact.currency)
        .await
        .map_err(|e| e.1)?;
    let say = |usd: f64| {
        let micros = (usd.abs() * rate as f64).round() as u128;
        let text = markets::say_micros(micros, &contact.currency);
        if usd < 0.0 {
            format!("-{text}")
        } else {
            text
        }
    };
    let fills = client
        .fills_since(wallet, contact.perps_seen_ms + 1)
        .await
        .map_err(|e| e.to_string())?;
    let mut seen = contact.perps_seen_ms;
    let mut done_oids = HashSet::new();
    let mut done_liquidations = HashSet::new();
    for fill in &fills {
        seen = seen.max(fill.time_ms);
        let symbol = fill
            .coin
            .split(':')
            .next_back()
            .unwrap_or(&fill.coin)
            .to_string();
        if fill.liquidated {
            if !done_liquidations.insert(fill.coin.clone()) {
                continue;
            }
            let lost: f64 = fills
                .iter()
                .filter(|f| f.liquidated && f.coin == fill.coin)
                .map(|f| f.closed_pnl)
                .sum();
            let message = Message {
                subject: format!("Your {symbol} position was liquidated"),
                lead: format!(
                    "{symbol} reached your liquidation price, so your position was closed. The margin in it is gone, but nothing else in your balance was touched."
                ),
                heading: "Liquidated".into(),
                amount: Some(say(lost)),
                lines: vec![
                    ("Market".into(), symbol.clone()),
                    ("Closed at".into(), format!("${}", trim_price(fill.price))),
                ],
                warning: true,
                link: SITE.into(),
            };
            let key = format!("liq:{user_id}:{}:{}", fill.coin, fill.time_ms / 600_000);
            state
                .emails
                .deliver(user_id, &key, |c| Some(render(c.name.as_deref(), &message)))
                .await;
            continue;
        }
        if !fill.dir.starts_with("Close") || !done_oids.insert(fill.oid) {
            continue;
        }
        let kind = client
            .order_type(wallet, fill.oid)
            .await
            .unwrap_or_default();
        let take_profit = kind.starts_with("Take Profit");
        if !take_profit && !kind.starts_with("Stop") {
            continue;
        }
        let pnl: f64 = fills
            .iter()
            .filter(|f| f.oid == fill.oid)
            .map(|f| f.closed_pnl)
            .sum();
        let message = Message {
            subject: if take_profit {
                format!("Take profit hit on {symbol}")
            } else {
                format!("Stop loss hit on {symbol}")
            },
            lead: if take_profit {
                format!("{symbol} reached your take profit, so your position closed with a gain. The cash stays in your Atlas balance.")
            } else {
                format!("{symbol} reached your stop loss, so your position closed before it could lose more. What's left stays in your Atlas balance.")
            },
            heading: if take_profit {
                "Take profit"
            } else {
                "Stop loss"
            }
            .into(),
            amount: Some(if pnl >= 0.0 {
                format!("+{}", say(pnl))
            } else {
                say(pnl)
            }),
            lines: vec![
                ("Market".into(), symbol.clone()),
                ("Closed at".into(), format!("${}", trim_price(fill.price))),
            ],
            warning: !take_profit,
            link: SITE.into(),
        };
        let key = format!("tpsl:{user_id}:{}", fill.oid);
        state
            .emails
            .deliver(user_id, &key, |c| Some(render(c.name.as_deref(), &message)))
            .await;
    }
    // Close to liquidation: once per position every 12 hours.
    let (main, on_dex) = tokio::join!(
        client.account(wallet),
        client.account_on(wallet, DEXES[0].0)
    );
    let main = main.map_err(|e| e.to_string())?;
    let on_dex = on_dex.unwrap_or_default();
    let positions: Vec<_> = main.positions.iter().chain(&on_dex.positions).collect();
    for p in &positions {
        let (Some(liq), Some(mark)) = (p.liquidation, hl::mark_of(state, &p.coin)) else {
            continue;
        };
        if !near_liquidation(p.entry, mark, liq, p.size > 0.0) {
            continue;
        }
        let symbol = p.coin.split(':').next_back().unwrap_or(&p.coin).to_string();
        let message = Message {
            subject: format!("Your {symbol} position is close to liquidation"),
            lead: format!(
                "{symbol} is at ${}, close to your liquidation price of ${}. If it gets there, the margin in this position is lost. You can close it, or part of it, in Atlas, or set a stop loss.",
                trim_price(mark),
                trim_price(liq)
            ),
            heading: "Heads up".into(),
            amount: None,
            lines: vec![
                ("Market".into(), symbol.clone()),
                ("Side".into(), format!("{} {}x", if p.size > 0.0 { "Long" } else { "Short" }, p.leverage)),
                ("Now".into(), format!("${}", trim_price(mark))),
                ("Liquidation".into(), format!("${}", trim_price(liq))),
                ("Profit/loss".into(), say(p.unrealized_pnl)),
            ],
            warning: true,
            link: SITE.into(),
        };
        let key = format!("nearliq:{user_id}:{}:{}", p.coin, now() / HALF_DAY_MS);
        state
            .emails
            .deliver(user_id, &key, |c| Some(render(c.name.as_deref(), &message)))
            .await;
    }
    state
        .emails
        .seen_perps(user_id, seen, !positions.is_empty())
        .await
        .map_err(|e| e.1)
}

// Past 70% of the way from entry to liquidation (and not yet beyond it).
fn near_liquidation(entry: f64, mark: f64, liquidation: f64, long: bool) -> bool {
    let room = (entry - liquidation).abs();
    if room <= 0.0 {
        return false;
    }
    let left = if long {
        mark - liquidation
    } else {
        liquidation - mark
    };
    left > 0.0 && left / room < NEAR_LIQUIDATION
}

// "120,000" or "0.5321".
fn trim_price(price: f64) -> String {
    let text = engine_execution::hyperliquid::order_price(price, 0);
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let mut grouped = String::new();
    for (i, digit) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if fraction.is_empty() {
        grouped
    } else {
        format!("{grouped}.{fraction}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn told(kind: &str, state: &str, lines: &[(&str, &str)]) -> transactions::Told {
        transactions::Told {
            id: "abc".into(),
            kind: kind.into(),
            symbol: "SOL".into(),
            state: state.into(),
            error: None,
            lines: lines
                .iter()
                .map(|(l, v)| ((*l).into(), (*v).into()))
                .collect(),
            units: Some(10_000_000),
        }
    }

    #[test]
    fn emails_greet_by_first_name_from_ebube() {
        let m = receipt_message(
            &told("onramp", "filled", &[("Status", "Added to your balance")]),
            Some("₦15,000.00".into()),
        )
        .unwrap();
        assert_eq!(m.subject, "₦15,000.00 added to your Atlas balance");
        let email = render(Some("EBUKA okafor"), &m);
        assert!(email
            .text
            .starts_with("Hi Ebuka,\n\nIt's Ebube from Atlas. Your bank transfer arrived"));
        assert!(email.html.contains("Hi Ebuka,"));
        assert!(email.html.contains(PINK));
        assert!(!email.html.contains("Added to your balance"));
        assert!(render(None, &m).text.starts_with("Hi there,"));
        assert_eq!(first_name(Some("  ")), "there");
    }

    #[test]
    fn what_users_wrote_is_escaped() {
        let m = receipt_message(
            &told("send", "filled", &[("Send to", "<script>@x</script>")]),
            Some("$10.00".into()),
        )
        .unwrap();
        let email = render(Some("<b>Ann</b>"), &m);
        assert!(!email.html.contains("<script>"));
        assert!(!email.html.contains("<b>Ann"));
    }

    #[test]
    fn only_news_is_emailed() {
        // Seen in the app as it happened: no email for a failed buy, or an expired top-up.
        assert!(receipt_message(&told("buy", "failed", &[]), None).is_none());
        assert!(
            receipt_message(&told("onramp", "failed", &[("Status", "Expired")]), None).is_none()
        );
        assert!(
            receipt_message(&told("onramp", "failed", &[("Status", "Failed")]), None).is_some()
        );
        // A bank cash-out is news when the bank is paid, not when the USDC left.
        assert!(receipt_message(
            &told("offramp", "filled", &[("Bank payout", "Paying your bank")]),
            None
        )
        .is_none());
        let paid = receipt_message(
            &told(
                "offramp",
                "filled",
                &[
                    ("Bank payout", "Paid ₦49,500.00"),
                    ("Bank gets", "₦49,500.00"),
                ],
            ),
            None,
        )
        .unwrap();
        assert_eq!(paid.subject, "₦49,500.00 sent to your bank");
        assert!(
            receipt_message(
                &told(
                    "offramp",
                    "filled",
                    &[("Bank payout", "Failed — contact support")]
                ),
                None
            )
            .unwrap()
            .warning
        );
        let friend = receipt_message(
            &told("send", "filled", &[("Send to", "@bob")]),
            Some("₦5,000.00".into()),
        )
        .unwrap();
        assert_eq!(friend.subject, "You sent ₦5,000.00 to @bob");
        let mut prediction = told(
            "buy",
            "filled",
            &[("Market", "Will it rain?"), ("Your choice", "Yes")],
        );
        prediction.id = "prediction-1".into();
        assert_eq!(
            receipt_message(&prediction, None).unwrap().lead,
            "You're in on \"Will it rain?\" (Yes)."
        );
        assert_eq!(
            receipt_message(&told("perp_open", "filled", &[("Action", "Long 5x")]), None)
                .unwrap()
                .lead,
            "Your long 5x on SOL is open."
        );
    }

    #[test]
    fn close_to_liquidation_means_most_of_the_room_is_gone() {
        // A long from 100, liquidated at 80: 85 has 25% of the room left.
        assert!(near_liquidation(100.0, 85.0, 80.0, true));
        assert!(!near_liquidation(100.0, 95.0, 80.0, true));
        assert!(!near_liquidation(100.0, 79.0, 80.0, true));
        // A short from 100, liquidated at 120.
        assert!(near_liquidation(100.0, 115.0, 120.0, false));
        assert!(!near_liquidation(100.0, 101.0, 120.0, false));
        assert_eq!(trim_price(120_000.0), "120,000");
    }

    #[tokio::test]
    async fn each_event_is_sent_once() {
        let emails = EmailState {
            key: None,
            from: "hello@justatlas.xyz".into(),
            from_name: "Ebube from Atlas".into(),
            http: reqwest::Client::new(),
            postgres: None,
            memory: Arc::new(Mutex::new(Memory::default())),
            remembered: Arc::new(Mutex::new(HashMap::new())),
            handled: Arc::new(Mutex::new(HashSet::new())),
        };
        assert!(emails.claim("tx:1:filled").await.unwrap());
        assert!(!emails.claim("tx:1:filled").await.unwrap());
        emails.release("tx:1:filled").await;
        assert!(emails.claim("tx:1:filled").await.unwrap());
        let user = app_balance::VerifiedWallets {
            user_id: "did:privy:a".into(),
            evm_wallet: Some("0xabc".into()),
            solana_wallet: None,
            email: Some("ada@example.com".into()),
            name: Some("Ada".into()),
        };
        emails.remember(&user, Some("USD")).await.unwrap();
        let c = emails.contact("did:privy:a").await.unwrap().unwrap();
        assert!(c.enabled);
        assert_eq!(
            (c.email.as_str(), c.currency.as_str()),
            ("ada@example.com", "USD")
        );
        assert!(emails.set_enabled("did:privy:a", false).await.unwrap());
        assert!(
            !emails
                .contact("did:privy:a")
                .await
                .unwrap()
                .unwrap()
                .enabled
        );
        assert!(!emails.set_enabled("did:privy:nobody", false).await.unwrap());
        emails.watch_perps("did:privy:a").await.unwrap();
        // Turned off: not watched for alerts.
        assert!(emails.watched().await.unwrap().is_empty());
    }
}
