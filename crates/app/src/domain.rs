//! The invoicing world. One handler set; every door calls into here.
//!
//! Everything is in memory on purpose: the oracle needs a seeded reset that is
//! instant and exact, and the benchmark never needs durability.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Customer {
    pub id: u64,
    pub name: String,
    pub email: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InvoiceStatus {
    Draft,
    Sent,
    Paid,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Invoice {
    pub id: u64,
    pub customer_id: u64,
    pub amount_cents: i64,
    pub status: InvoiceStatus,
    pub approved: bool,
    pub receipt_sent: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OutboxMessage {
    pub id: u64,
    pub to: String,
    pub subject: String,
    pub kind: String,
    pub invoice_id: u64,
    pub key: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Report {
    pub id: u64,
    pub invoice_ids: Vec<u64>,
    pub total_cents: i64,
    pub sent_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Payment {
    pub id: u64,
    pub invoice_id: u64,
}

/// One row per write attempt. The report counts double-sends here, never from an arm's own word.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Effect {
    pub seq: u64,
    pub at_ms: u128,
    pub op: String,
    pub door: String,
    pub key: Option<String>,
    pub entity: String,
    pub replayed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub at_ms: u128,
    pub kind: String,
    pub entity: String,
    pub id: u64,
    pub data: serde_json::Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Chaos {
    /// 0.0 to 1.0. Probability that a send returns 500 after doing nothing.
    #[serde(default)]
    pub send_fail_rate: f64,
    /// Added to every write, in milliseconds.
    #[serde(default)]
    pub latency_ms: u64,
    /// When set, send refuses unless the invoice was approved in the UI.
    #[serde(default)]
    pub require_approval: bool,
    /// When set, the UI shows a random modal in front of the invoice list.
    #[serde(default)]
    pub ui_modal: bool,
    /// Writes per second before a 429. 0 means unlimited.
    #[serde(default)]
    pub rate_limit_per_sec: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub seed: u64,
    pub customers: Vec<Customer>,
    pub invoices: Vec<Invoice>,
    pub outbox: Vec<OutboxMessage>,
    pub reports: Vec<Report>,
    pub payments: Vec<Payment>,
    pub chaos: Chaos,
}

#[derive(Debug)]
pub enum DomainError {
    NotFound(&'static str, u64),
    NotApproved(u64),
    NotPaid(u64),
    Ambiguous(String, usize),
    ChaosFail,
    RateLimited { retry_after_ms: u64 },
}

impl DomainError {
    /// How long a caller should wait before trying again, when the server knows.
    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            DomainError::RateLimited { retry_after_ms } => Some(*retry_after_ms),
            _ => None,
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            DomainError::NotFound(..) => 404,
            DomainError::NotApproved(_) | DomainError::NotPaid(_) => 409,
            DomainError::Ambiguous(..) => 409,
            DomainError::ChaosFail => 500,
            DomainError::RateLimited { .. } => 429,
        }
    }
    pub fn message(&self) -> String {
        match self {
            DomainError::NotFound(what, id) => format!("{what} {id} not found"),
            DomainError::NotApproved(id) => format!("invoice {id} is not approved; approval is UI-only"),
            DomainError::NotPaid(id) => format!("invoice {id} has no payment yet"),
            DomainError::Ambiguous(name, n) => format!("{n} customers match '{name}'"),
            DomainError::ChaosFail => "chaos: send failed before doing anything".to_string(),
            DomainError::RateLimited { retry_after_ms } => format!("chaos: rate limited, retry after {retry_after_ms} ms"),
        }
    }
}

pub type DomainResult<T> = Result<T, DomainError>;

pub fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// The whole world behind one lock. Handlers are short, so a mutex is fine.
pub struct World {
    pub seed: u64,
    pub customers: Vec<Customer>,
    pub invoices: Vec<Invoice>,
    pub outbox: Vec<OutboxMessage>,
    pub reports: Vec<Report>,
    pub payments: Vec<Payment>,
    pub effects: Vec<Effect>,
    pub events: Vec<Event>,
    pub chaos: Chaos,
    /// Idempotency: key -> the JSON response given the first time.
    pub replay: HashMap<String, serde_json::Value>,
    rng: ChaCha8Rng,
    next_id: u64,
    seq: u64,
    rate_window_start_ms: u128,
    rate_window_count: u32,
}

impl World {
    pub fn seeded(seed: u64) -> Self {
        let mut w = World {
            seed,
            customers: Vec::new(),
            invoices: Vec::new(),
            outbox: Vec::new(),
            reports: Vec::new(),
            payments: Vec::new(),
            effects: Vec::new(),
            events: Vec::new(),
            chaos: Chaos::default(),
            replay: HashMap::new(),
            rng: ChaCha8Rng::seed_from_u64(seed),
            next_id: 1,
            seq: 0,
            rate_window_start_ms: 0,
            rate_window_count: 0,
        };
        w.seed_customers();
        w
    }

    fn seed_customers(&mut self) {
        // Ten customers with distinct names. Seed 6 adds a second "Acme" for the fork task.
        const NAMES: [&str; 10] = ["Acme", "Globex", "Initech", "Umbrella", "Hooli", "Vandelay", "Stark", "Wayne", "Wonka", "Tyrell"];
        for name in NAMES {
            let id = self.alloc();
            self.customers.push(Customer { id, name: name.to_string(), email: format!("billing@{}.example", name.to_lowercase()) });
        }
        if self.seed % 100 == 6 {
            let id = self.alloc();
            self.customers.push(Customer { id, name: "Acme".into(), email: "ap@acme-holdings.example".into() });
        }
        // Seed 11: three hundred more, for the fan-out task.
        if self.seed % 100 == 11 {
            for i in 1..=300 {
                let id = self.alloc();
                self.customers.push(Customer { id, name: format!("Customer {i:03}"), email: format!("ap{i:03}@bulk.example") });
            }
        }
    }

    fn alloc(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            seed: self.seed,
            customers: self.customers.clone(),
            invoices: self.invoices.clone(),
            outbox: self.outbox.clone(),
            reports: self.reports.clone(),
            payments: self.payments.clone(),
            chaos: self.chaos.clone(),
        }
    }

    fn record_effect(&mut self, op: &str, door: &str, key: Option<&str>, entity: String, replayed: bool) {
        let seq = self.next_seq();
        self.effects.push(Effect { seq, at_ms: now_ms(), op: op.to_string(), door: door.to_string(), key: key.map(|k| k.to_string()), entity, replayed });
    }

    fn emit(&mut self, kind: &str, entity: &str, id: u64, data: serde_json::Value) -> Event {
        let seq = self.next_seq();
        let ev = Event { seq, at_ms: now_ms(), kind: kind.to_string(), entity: entity.to_string(), id, data };
        self.events.push(ev.clone());
        ev
    }

    /// Chaos gate shared by every write. Returns the latency to sleep, or an error.
    fn write_gate(&mut self, op: &str) -> DomainResult<u64> {
        if self.chaos.rate_limit_per_sec > 0 {
            let now = now_ms();
            if now.saturating_sub(self.rate_window_start_ms) >= 1000 {
                self.rate_window_start_ms = now;
                self.rate_window_count = 0;
            }
            self.rate_window_count += 1;
            if self.rate_window_count > self.chaos.rate_limit_per_sec {
                self.record_effect(op, "chaos", None, "rate_limited".into(), false);
                let remaining = 1000u128.saturating_sub(now.saturating_sub(self.rate_window_start_ms)) as u64;
                return Err(DomainError::RateLimited { retry_after_ms: remaining.max(1) });
            }
        }
        Ok(self.chaos.latency_ms)
    }

    // ---- reads ----

    pub fn find_customers(&self, name: Option<&str>) -> Vec<Customer> {
        match name {
            Some(n) => self.customers.iter().filter(|c| c.name.eq_ignore_ascii_case(n)).cloned().collect(),
            None => self.customers.clone(),
        }
    }

    pub fn find_customers_by_prefix(&self, prefix: &str) -> Vec<Customer> {
        self.customers.iter().filter(|c| c.name.to_lowercase().starts_with(&prefix.to_lowercase())).cloned().collect()
    }

    pub fn resolve_customer(&self, name: &str) -> DomainResult<Customer> {
        let found = self.find_customers(Some(name));
        match found.len() {
            1 => Ok(found[0].clone()),
            0 => Err(DomainError::NotFound("customer", 0)),
            n => Err(DomainError::Ambiguous(name.to_string(), n)),
        }
    }

    pub fn invoice(&self, id: u64) -> DomainResult<Invoice> {
        self.invoices.iter().find(|i| i.id == id).cloned().ok_or(DomainError::NotFound("invoice", id))
    }

    pub fn list_invoices(&self, customer_id: Option<u64>) -> Vec<Invoice> {
        self.invoices.iter().filter(|i| customer_id.is_none_or(|c| i.customer_id == c)).cloned().collect()
    }

    // ---- writes. Each returns (json, latency_ms, events) ----

    pub fn create_customer(&mut self, door: &str, key: Option<&str>, name: &str, email: &str) -> DomainResult<(serde_json::Value, u64, Vec<Event>)> {
        if let Some((v, lat)) = self.replayed("createCustomer", door, key) {
            return Ok((v, lat, vec![]));
        }
        let lat = self.write_gate("createCustomer")?;
        let id = self.alloc();
        let c = Customer { id, name: name.to_string(), email: email.to_string() };
        self.customers.push(c.clone());
        self.record_effect("createCustomer", door, key, format!("customer:{id}"), false);
        let ev = self.emit("customer.created", "customer", id, serde_json::to_value(&c).unwrap());
        let v = serde_json::to_value(&c).unwrap();
        self.remember(key, &v);
        Ok((v, lat, vec![ev]))
    }

    pub fn create_invoice(&mut self, door: &str, key: Option<&str>, customer_id: u64, amount_cents: i64) -> DomainResult<(serde_json::Value, u64, Vec<Event>)> {
        if let Some((v, lat)) = self.replayed("createInvoice", door, key) {
            return Ok((v, lat, vec![]));
        }
        if !self.customers.iter().any(|c| c.id == customer_id) {
            return Err(DomainError::NotFound("customer", customer_id));
        }
        let lat = self.write_gate("createInvoice")?;
        let id = self.alloc();
        let inv = Invoice { id, customer_id, amount_cents, status: InvoiceStatus::Draft, approved: false, receipt_sent: false };
        self.invoices.push(inv.clone());
        self.record_effect("createInvoice", door, key, format!("invoice:{id}"), false);
        let ev = self.emit("invoice.created", "invoice", id, serde_json::to_value(&inv).unwrap());
        let v = serde_json::to_value(&inv).unwrap();
        self.remember(key, &v);
        Ok((v, lat, vec![ev]))
    }

    /// UI-only. There is no API operation for this on purpose.
    pub fn approve_invoice(&mut self, id: u64) -> DomainResult<(serde_json::Value, Vec<Event>)> {
        let idx = self.invoices.iter().position(|i| i.id == id).ok_or(DomainError::NotFound("invoice", id))?;
        self.invoices[idx].approved = true;
        let inv = self.invoices[idx].clone();
        self.record_effect("approveInvoice", "ui", None, format!("invoice:{id}"), false);
        let ev = self.emit("invoice.approved", "invoice", id, serde_json::to_value(&inv).unwrap());
        Ok((serde_json::to_value(&inv).unwrap(), vec![ev]))
    }

    pub fn send_invoice(&mut self, door: &str, key: Option<&str>, id: u64) -> DomainResult<(serde_json::Value, u64, Vec<Event>)> {
        if let Some((v, lat)) = self.replayed("sendInvoice", door, key) {
            return Ok((v, lat, vec![]));
        }
        let idx = self.invoices.iter().position(|i| i.id == id).ok_or(DomainError::NotFound("invoice", id))?;
        if self.chaos.require_approval && !self.invoices[idx].approved {
            return Err(DomainError::NotApproved(id));
        }
        let lat = self.write_gate("sendInvoice")?;
        if self.chaos.send_fail_rate > 0.0 && self.rng.random::<f64>() < self.chaos.send_fail_rate {
            self.record_effect("sendInvoice", door, key, format!("invoice:{id} (chaos fail)"), false);
            return Err(DomainError::ChaosFail);
        }
        let customer = self.customers.iter().find(|c| c.id == self.invoices[idx].customer_id).cloned();
        let to = customer.map(|c| c.email).unwrap_or_default();
        let mid = self.alloc();
        // Sending never regresses a paid invoice to "sent"; payment is the later fact.
        if self.invoices[idx].status == InvoiceStatus::Draft {
            self.invoices[idx].status = InvoiceStatus::Sent;
        }
        self.outbox.push(OutboxMessage {
            id: mid,
            to,
            subject: format!("Invoice #{id}"),
            kind: "invoice".into(),
            invoice_id: id,
            key: key.map(|k| k.to_string()),
        });
        let inv = self.invoices[idx].clone();
        self.record_effect("sendInvoice", door, key, format!("invoice:{id},outbox:{mid}"), false);
        let ev1 = self.emit("invoice.sent", "invoice", id, serde_json::to_value(&inv).unwrap());
        let ev2 = self.emit("outbox.queued", "outbox", mid, serde_json::json!({"invoice_id": id}));
        let v = serde_json::to_value(&inv).unwrap();
        self.remember(key, &v);
        Ok((v, lat, vec![ev1, ev2]))
    }

    /// Only valid after a payment landed. This is the wait-for-event task.
    pub fn send_receipt(&mut self, door: &str, key: Option<&str>, id: u64) -> DomainResult<(serde_json::Value, u64, Vec<Event>)> {
        if let Some((v, lat)) = self.replayed("sendReceipt", door, key) {
            return Ok((v, lat, vec![]));
        }
        let idx = self.invoices.iter().position(|i| i.id == id).ok_or(DomainError::NotFound("invoice", id))?;
        if self.invoices[idx].status != InvoiceStatus::Paid {
            return Err(DomainError::NotPaid(id));
        }
        let lat = self.write_gate("sendReceipt")?;
        let to = self.customers.iter().find(|c| c.id == self.invoices[idx].customer_id).map(|c| c.email.clone()).unwrap_or_default();
        let mid = self.alloc();
        self.invoices[idx].receipt_sent = true;
        self.outbox.push(OutboxMessage {
            id: mid,
            to,
            subject: format!("Receipt for invoice #{id}"),
            kind: "receipt".into(),
            invoice_id: id,
            key: key.map(|k| k.to_string()),
        });
        let inv = self.invoices[idx].clone();
        self.record_effect("sendReceipt", door, key, format!("invoice:{id},outbox:{mid}"), false);
        let ev = self.emit("receipt.sent", "invoice", id, serde_json::to_value(&inv).unwrap());
        let v = serde_json::to_value(&inv).unwrap();
        self.remember(key, &v);
        Ok((v, lat, vec![ev]))
    }

    pub fn create_report(&mut self, door: &str, key: Option<&str>, invoice_ids: &[u64]) -> DomainResult<(serde_json::Value, u64, Vec<Event>)> {
        if let Some((v, lat)) = self.replayed("createReport", door, key) {
            return Ok((v, lat, vec![]));
        }
        let mut total = 0;
        let mut sent = 0;
        for id in invoice_ids {
            let inv = self.invoice(*id)?;
            total += inv.amount_cents;
            if inv.status != InvoiceStatus::Draft {
                sent += 1;
            }
        }
        let lat = self.write_gate("createReport")?;
        let id = self.alloc();
        let r = Report { id, invoice_ids: invoice_ids.to_vec(), total_cents: total, sent_count: sent };
        self.reports.push(r.clone());
        self.record_effect("createReport", door, key, format!("report:{id}"), false);
        let ev = self.emit("report.created", "report", id, serde_json::to_value(&r).unwrap());
        let v = serde_json::to_value(&r).unwrap();
        self.remember(key, &v);
        Ok((v, lat, vec![ev]))
    }

    /// The outside world paying. Fired by the oracle, never by an arm. `silent` models a lost
    /// webhook: the payment lands, no event is emitted.
    pub fn receive_payment(&mut self, invoice_id: u64, silent: bool) -> DomainResult<(serde_json::Value, Vec<Event>)> {
        let idx = self.invoices.iter().position(|i| i.id == invoice_id).ok_or(DomainError::NotFound("invoice", invoice_id))?;
        let id = self.alloc();
        self.invoices[idx].status = InvoiceStatus::Paid;
        let p = Payment { id, invoice_id };
        self.payments.push(p.clone());
        self.record_effect("receivePayment", "webhook", None, format!("payment:{id}"), false);
        if silent {
            return Ok((serde_json::to_value(&p).unwrap(), vec![]));
        }
        let ev = self.emit("payment.received", "invoice", invoice_id, serde_json::to_value(&p).unwrap());
        Ok((serde_json::to_value(&p).unwrap(), vec![ev]))
    }

    fn replayed(&mut self, op: &str, door: &str, key: Option<&str>) -> Option<(serde_json::Value, u64)> {
        let k = key?;
        let v = self.replay.get(k)?.clone();
        self.record_effect(op, door, Some(k), "replayed".into(), true);
        Some((v, 0))
    }

    fn remember(&mut self, key: Option<&str>, v: &serde_json::Value) {
        if let Some(k) = key {
            self.replay.insert(k.to_string(), v.clone());
        }
    }

    /// Outbox rows that hit the same invoice with the same kind more than once.
    pub fn double_sends(&self) -> usize {
        let mut seen: HashMap<(u64, &str), usize> = HashMap::new();
        for m in &self.outbox {
            *seen.entry((m.invoice_id, m.kind.as_str())).or_default() += 1;
        }
        seen.values().filter(|n| **n > 1).map(|n| n - 1).sum()
    }
}
