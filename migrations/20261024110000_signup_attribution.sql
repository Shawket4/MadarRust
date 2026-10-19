-- Where a self-signed-up shop came from: the campaign tags on the signup page's
-- URL (utm_*, fbclid, gclid), carried there from the landing. Lets a campaign be
-- joined to plan and retention later, ad blocker or not. NULL for admin-made orgs
-- and for a signup with no tags. Written once by POST /public/signup, never read
-- by the app.
ALTER TABLE organizations ADD COLUMN signup_attribution jsonb NULL;
