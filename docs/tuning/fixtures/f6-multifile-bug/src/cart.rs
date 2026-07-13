//! The shopping cart: an ordered collection of items.

use crate::item::Item;

#[derive(Debug, Default, Clone)]
pub struct Cart {
    items: Vec<Item>,
}

impl Cart {
    pub fn new() -> Cart {
        Cart::default()
    }

    pub fn add(&mut self, item: Item) {
        self.items.push(item);
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Sum of every line's extended price, in **cents**.
    pub fn subtotal_cents(&self) -> u64 {
        self.items.iter().map(Item::total_cents).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cart_has_zero_subtotal() {
        assert_eq!(Cart::new().subtotal_cents(), 0);
    }

    #[test]
    fn subtotal_sums_line_totals_in_cents() {
        let mut cart = Cart::new();
        cart.add(Item::new("coffee", 250, 2)); // 500
        cart.add(Item::new("mug", 1_200, 1)); // 1200
        assert_eq!(cart.subtotal_cents(), 1_700);
    }

    #[test]
    fn add_grows_the_cart() {
        let mut cart = Cart::new();
        assert!(cart.is_empty());
        cart.add(Item::new("coffee", 250, 1));
        assert_eq!(cart.len(), 1);
        assert_eq!(cart.items()[0].name, "coffee");
    }
}
