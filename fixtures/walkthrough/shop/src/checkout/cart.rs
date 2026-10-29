pub struct CartService;

impl CartService {
    pub fn apply_discount(total: u64, discount: u64) -> u64 {
        total.saturating_sub(discount)
    }
}

pub struct Cart;
pub struct CartLine;
pub struct CartItem;
pub struct CartTotal;
pub struct CartTax;
pub struct CartCoupon;
