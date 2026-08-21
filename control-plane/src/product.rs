//! Reserved product-App namespace. No route is installed until signed
//! deliveries can enter the provider-neutral quarantine without executable
//! authority.

pub const WEBHOOK_PATH: &str = "/v1/github/product/webhook";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductWebhookState {
    Inactive,
}
