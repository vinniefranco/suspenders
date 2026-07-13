//! A purchasable line item.

/// One line item in a cart. `price_cents` is the unit price in cents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub price_cents: u64,
    pub qty: u64,
}

impl Item {
    pub fn new(name: &str, price_cents: u64, qty: u64) -> Item {
        Item {
            name: name.to_string(),
            price_cents,
            qty,
        }
    }

    /// Extended price for the line: unit price times quantity, in cents.
    pub fn total_cents(&self) -> u64 {
        self.price_cents * self.qty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_is_price_times_qty() {
        let item = Item::new("coffee", 250, 3);
        assert_eq!(item.total_cents(), 750);
    }

    #[test]
    fn zero_qty_costs_nothing() {
        let item = Item::new("placeholder", 9_999, 0);
        assert_eq!(item.total_cents(), 0);
    }
}
