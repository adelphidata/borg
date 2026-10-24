//! Subscription endpoint and channel tap.
//!
//! `POST /v1/events/{id}` stores events from outside services (GitHub,
//! Sentry, Stripe, inbound email, any JSON sender); the channel tap stores
//! matching channel messages. Both only write rows to the event ledger and
//! poke the dispatcher in `borgd`, which runs the agent.

pub mod routes;
pub mod tap;
pub mod verify;

pub use routes::{router, EventsState, ENDPOINT_PREFIX};
