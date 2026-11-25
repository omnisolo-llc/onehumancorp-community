# Viral Referral & Discount Widget Configuration
Keywords: referral fab, discount code, coupon, secret menu, flash sale, spin to win, waitlist, giveaway, promo widget

## Scope

Covers customer-incentive widgets that affect pricing, margins, or prize fulfillment:
- `referral_fab` (Floating Referral FAB script)
- `discount_code` (Discount Code Embed iframe)
- `secret_menu` (Share-to-Unlock Secret Menu iframe)
- `flash_sale_countdown` (Flash Sale Countdown iframe)
- `spin_to_win` (Spin-to-Win Wheel iframe)
- `waitlist_referral` (Viral Referral Waitlist iframe)
- `giveaway_widget` (Viral Giveaway & Challenge iframe)

## Mandatory Margin & Standing-Authority Checks

1. Before calling `viral_widgets.configure`, confirm with the owner:
   - Maximum discount percentage or dollar reward and whether it stacks with existing promos.
   - Exact coupon/access code (`[A-Za-z0-9_-]`, max 32 chars) and whether that code is active in checkout/POS.
   - Start/end window (`duration_hours` bounded `1..=720`) or entry cap (`max_entries` bounded `1..=100000`).
2. Always run `viral_widgets.get_schema` first so bounds and defaults are verified against the live catalog.
3. Always save a draft with `"owner_confirmed": false` first; never set `"owner_confirmed": true` until the owner explicitly approves the exact discount/reward copy and redemption code.

## Exact MCP Invocation Sequence

1. `viral_widgets.list` with `{ "category": "referral" }` or `{ "category": "conversion" }` or `{ "category": "gamification" }`.
2. `viral_widgets.get_schema` with `{ "widget_id": "<id>" }`.
3. `viral_widgets.configure` with `{ "tenant_id": "<tenant_id>", "widget_id": "<id>", "owner_confirmed": false, "params": { ... } }`.
4. Present preview URL and terms; wait for explicit owner approval.
5. `viral_widgets.configure` with `"owner_confirmed": true`, followed by `viral_widgets.generate_embed` if custom width/height/title is requested.
