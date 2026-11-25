# Viral Booking & Social Proof Widget Configuration
Keywords: booking embed, booking widget, milestone card, testimonial, before after slider, interactive poll, tip jar, community goal, tier list, leaderboard

## Scope

Covers scheduling, trust, and audience-engagement widgets:
- `booking_embed` (Service Booking Card iframe)
- `general_embed_widget` (Quote / Booking / Contact / Leaderboard iframe)
- `milestone_card` (Shareable Milestone SVG Card image)
- `before_after_slider` (Before & After Visual Slider iframe)
- `testimonial_card` (Customer Testimonial Card iframe)
- `interactive_poll` (Interactive Customer Poll iframe)
- `tip_jar` (Digital Tip Jar iframe)
- `community_goal` (Community Goal Tracker iframe)
- `viral_tier_list` (Shareable Tier List Widget iframe)

## Accessibility, Consent & Content Safety Rules

1. **Consent & Truthfulness**:
   - Confirm customer consent before embedding real quotes (`testimonial_card`) or before/after photos (`before_after_slider`, which strictly requires public `https://` image URLs).
   - Never fabricate milestones or community counts.
2. **WCAG 2.2 AA & CSP Hygiene**:
   - Provide a clear, descriptive accessible `title` when calling `viral_widgets.generate_embed` (used as `<iframe title="...">` or `<img alt="...">`).
   - Prefer high-contrast hex colors (`>= 4.5:1` contrast against white text on buttons) and verify light/dark theme alignment with the host website.
   - Keep `sandbox` and `referrerpolicy="strict-origin-when-cross-origin"` intact on every generated snippet.
3. **Owner Confirmation**:
   - Draft first via `viral_widgets.configure` (`"owner_confirmed": false`), share `preview_url`, and finalize with `"owner_confirmed": true` only after owner sign-off.

## Exact MCP Invocation Sequence

1. `viral_widgets.list` with `{ "category": "booking" }` or `{ "category": "social_proof" }` or `{ "category": "gamification" }`.
2. `viral_widgets.get_schema` with `{ "widget_id": "<id>" }`.
3. `viral_widgets.configure` (`owner_confirmed: false`) -> review `preview_url` with owner.
4. `viral_widgets.configure` (`owner_confirmed: true`) -> `viral_widgets.generate_embed` with accessible `title` and target dimensions.
