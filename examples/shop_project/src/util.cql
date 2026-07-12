// Public pure-function module for the shop project
module util;

public function is_large_amount(x: float) -> bool == { x >= 100.0 }

public function clamp_nonnegative(x: float) -> float == {
    if x < 0.0 then 0.0 else x
}
