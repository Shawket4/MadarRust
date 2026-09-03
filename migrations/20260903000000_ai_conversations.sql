-- Server-side conversation history for the analytics chat.
--
-- Until now the conversation window was supplied by the CLIENT on every
-- request. That works, and it is what a stateless endpoint wants, but it buys
-- three problems: a chat cannot be resumed on another device, a client that
-- forgets to send `history` silently gets a model with no memory and no error
-- to say so, and the window can only ever be as long as the client is willing
-- to re-upload. Holding it here fixes all three and makes unlimited-but-
-- compacted context possible, which a client-supplied window cannot be.
--
-- ── Why one row per TURN, not per message ───────────────────────────────────
--
-- The conventional shape is one row per message with a `role` column. Here a
-- question always produces exactly one assistant reply — the agent loop is
-- internal and its intermediate tool calls are deliberately not persisted — so
-- role-split rows would add a join, an ordering rule, and a whole class of
-- orphaned half-turns, in exchange for nothing this system uses. A turn is the
-- atomic unit: it is what is replayed, what is summarized, and what a chat UI
-- renders as one exchange.
--
-- ── Why specs and not result rows ───────────────────────────────────────────
--
-- `specs` holds the QUERIES that answered, never the rows they returned. Rows
-- are large, they go stale the moment another order is rung up, and a reopened
-- conversation showing last week's numbers as if they were current would be
-- worse than showing nothing. Re-running the stored spec gives fresh figures
-- and costs one query. It is also exactly the value a client needs to pin an
-- answer to a dashboard, so one field serves both.

CREATE TABLE ai_conversations (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- A conversation belongs to ONE user. RLS below fences the org; the user
    -- fence lives in every query in `ai::store` because `app.user_id` is not a
    -- connection setting — the tenant pool is per-org, not per-user.
    user_id     uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title       text NOT NULL,
    locale      text NOT NULL DEFAULT 'en',

    -- ── Rolling compaction ──────────────────────────────────────────────────
    -- `summary` covers every turn up to and including `summarized_through_seq`;
    -- turns after it replay verbatim. Advancing the two together, and only
    -- together, is what keeps the replayed context complete: there is never a
    -- turn that is neither summarized nor replayed.
    summary                 text,
    summarized_through_seq  integer NOT NULL DEFAULT 0,

    -- `turn_count` is the sequence allocator as well as a display counter. It
    -- is bumped under a row lock so two concurrent sends in one conversation
    -- cannot claim the same `seq`.
    turn_count   integer NOT NULL DEFAULT 0,
    last_turn_at timestamptz,

    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    -- Soft delete: a merchant removing a chat should not orphan the analytics
    -- telemetry that references it, and undelete is a support request away.
    deleted_at timestamptz,

    CONSTRAINT ai_conversations_summary_covers_existing_turns
        CHECK (summarized_through_seq >= 0 AND summarized_through_seq <= turn_count)
);

CREATE INDEX ai_conversations_user_recent
    ON ai_conversations (user_id, last_turn_at DESC NULLS LAST)
    WHERE deleted_at IS NULL;

CREATE TABLE ai_messages (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    conversation_id uuid NOT NULL REFERENCES ai_conversations(id) ON DELETE CASCADE,
    -- Denormalized so RLS can fence this table directly instead of through a
    -- subquery on every read.
    org_id          uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    seq             integer NOT NULL,

    question text NOT NULL,
    -- The assistant's reply. For `clarify` this is the question it asked back.
    answer   text,
    -- How the turn ended: answer | clarify | incomplete. Kept as text rather
    -- than an enum so adding an outcome does not need a migration.
    kind     text NOT NULL,
    -- `[{title, preset_id, spec}]` — the queries that produced the answer.
    specs    jsonb NOT NULL DEFAULT '[]'::jsonb,
    provider text,

    created_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT ai_messages_seq_unique UNIQUE (conversation_id, seq),
    CONSTRAINT ai_messages_seq_positive CHECK (seq > 0),
    CONSTRAINT ai_messages_specs_is_array CHECK (jsonb_typeof(specs) = 'array')
);

CREATE INDEX ai_messages_conversation_seq ON ai_messages (conversation_id, seq);

-- ── Row-level security ──────────────────────────────────────────────────────
-- Same shape as every other org-rooted table: the policy binds `madar_app` to
-- the org set on the connection. See `migrations/*_rls_policies.sql`; its
-- completeness gate requires every base table to enforce row security, so a
-- new table without these two statements would fail a fresh migration run.
ALTER TABLE ai_conversations ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON ai_conversations FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

ALTER TABLE ai_messages ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON ai_messages FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

-- Pre-rebrand role that several applied migrations grant to; every table needs
-- it or a fresh database build fails. See the `sufrix` note in CLAUDE.md.
GRANT ALL ON TABLE ai_conversations TO sufrix;
GRANT ALL ON TABLE ai_messages TO sufrix;
