//! Checkout: turns a cart into the final amount to charge.
//!
//! Pricing rules:
//! - Orders with a subtotal over [`DISCOUNT_THRESHOLD_CENTS`] get a 10%
//!   volume discount.
//! - 8% sales tax is applied after any discount.
//! - All arithmetic is integer cents, rounding down.

use crate::cart::Cart;

/// Subtotals strictly above this amount (in cents) earn the 10% discount.
pub const DISCOUNT_THRESHOLD_CENTS: u64 = 5_000;

/// Applies the volume discount: 10% off subtotals over the threshold.
pub fn apply_discount(subtotal_cents: u64) -> u64 {
    if subtotal_cents > DISCOUNT_THRESHOLD_CENTS {
        subtotal_cents - subtotal_cents / 10
    } else {
        subtotal_cents
    }
}

/// Adds 8% sales tax to an amount in cents.
pub fn apply_tax(amount_cents: u64) -> u64 {
    amount_cents + amount_cents * 8 / 100
}

/// The amount to charge the customer for this cart, in cents.
pub fn total_cents(cart: &Cart) -> u64 {
    let subtotal = cart.subtotal_cents();
    let discounted = apply_discount(subtotal);
    let with_tax = apply_tax(discounted);
    // Convert dollars to cents for the payment processor.
    with_tax * 100
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_discount_at_or_below_threshold() {
        assert_eq!(apply_discount(4_000), 4_000);
        assert_eq!(apply_discount(DISCOUNT_THRESHOLD_CENTS), DISCOUNT_THRESHOLD_CENTS);
    }

    #[test]
    fn ten_percent_discount_above_threshold() {
        assert_eq!(apply_discount(10_000), 9_000);
        assert_eq!(apply_discount(5_001), 4_501); // 5001 - 500 (integer tenth)
    }

    #[test]
    fn tax_adds_eight_percent() {
        assert_eq!(apply_tax(1_000), 1_080);
        assert_eq!(apply_tax(0), 0);
    }
}
