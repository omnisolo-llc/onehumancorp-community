# Viral Widgets Configurator
Keywords: viral widget, widget, embed, referral fab, discount code, secret menu, milestone card, booking embed, flash sale, spin to win, before after slider, poll, waitlist, tip jar, testimonial, community goal, tier list, giveaway

## Purpose

Guide business owners through discovering, configuring, validating, confirming, and safely embedding OneHumanCorp viral growth widgets using the `viral_widgets.*` MCP tools.

## Standing-Authority & Safety Rules (Mandatory)

1. **Never publish customer-facing widgets, discounts, or monetary incentives without explicit owner confirmation.**
   - Always call `viral_widgets.configure` with `"owner_confirmed": false` first to generate a validated draft, preview URL, and review summary.
   - Present the exact customer-facing copy, discount percentage or dollar reward, redemption code, share threshold, and visual theme to the owner.
   - Only re-run `viral_widgets.configure` with `"owner_confirmed": true` after the owner explicitly confirms in the conversation (or via an approved standing-authority rule covering that exact incentive cap).
2. **Never fabricate business metrics, testimonials, or milestones.**
   - Verify real order counts, milestone attainment, and customer testimonial permission before configuring `milestone_card`, `testimonial_card`, or `community_goal`.
3. **Never hand-write unescaped `<script>` or `<iframe>` snippets.**
   - Always generate embed HTML exclusively via `viral_widgets.configure` or `viral_widgets.generate_embed`, which enforce strict fail-closed schema validation, percent-encoding, HTML attribute escaping, and `sandbox` + CSP recommendations.
4. **Enforce strict tenant isolation.**
   - Always bind `tenant_id` to the owner's authenticated organization ID. Never use `"default"`, `"system"`, or another tenant's identifier.

## Step-by-Step Owner Interview Workflow

### Step 1: Discover Candidate Widgets
- Call `viral_widgets.list` (optionally filtered by `category`: `referral`, `conversion`, `social_proof`, `gamification`, `booking`, or `query`).
- Recommend 1-3 widgets aligned with the owner's stated goal (e.g., new-customer acquisition -> `referral_fab` or `waitlist_referral`; checkout conversion -> `discount_code` or `flash_sale_countdown`; bookings -> `booking_embed`; proof -> `milestone_card` or `testimonial_card`).

### Step 2: Inspect Strict Parameter Schema
- Call `viral_widgets.get_schema` with `{ "widget_id": "<chosen_id>" }`.
- Review required fields, numeric caps (e.g., discount percentage `1..=90`, share threshold `1..=20`, spin-to-win max discount `1..=50`), hex color format (`#RRGGBB`), and example presets before asking the owner follow-up questions.

### Step 3: Interview the Owner for Missing Inputs
Ask concise, concrete questions for required parameters only:
- **Incentive / Offer**: Exact reward amount or discount cap, margin impact, and redemption code (if applicable).
- **Copy & Branding**: Headline, service/item name, primary hex brand color, and `light`/`dark` theme.
- **Placement**: Where the widget will be embedded (storefront homepage, booking page, checkout confirmation, email landing page) and preferred width/height.

### Step 4: Validate & Persist Draft (`owner_confirmed: false`)
- Call `viral_widgets.configure`:
  ```json
  {
    "tenant_id": "<authenticated_tenant_id>",
    "widget_id": "<chosen_id>",
    "owner_confirmed": false,
    "params": { "...": "..." }
  }
  ```
- If validation fails (e.g., invalid hex color, out-of-range discount, unsafe URL, or markup characters), explain the exact constraint clearly and collect corrected plain-text values.
- Share the returned `preview_url`, `validated_params`, and `standing_authority_note` with the owner and ask for explicit confirmation.

### Step 5: Confirm & Generate Final CSP-Safe Embed
- After explicit owner approval, call `viral_widgets.configure` with `"owner_confirmed": true`.
- If the owner wants custom iframe dimensions (`width`: `200..=1600`, `height`: `120..=1200`) or an accessible iframe `title`, call `viral_widgets.generate_embed` with `{ "tenant_id": "<authenticated_tenant_id>", "widget_id": "<chosen_id>", "width": ..., "height": ..., "title": "..." }`.
- Deliver the exact `embed_html`, `preview_url`, builder page route (`builder_route`), and `recommended_csp` note to the owner.
