CREATE TABLE generic_rules (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    scope TEXT NOT NULL,
    rule TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE entity_rules (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    entity_id TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_name TEXT,
    rule TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_entity_rules_entity ON entity_rules(entity_id);

CREATE TABLE decision_corrections (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    scope TEXT NOT NULL DEFAULT 'release-classification',
    release_name TEXT NOT NULL,
    entity_id TEXT,
    model_decision TEXT NOT NULL,
    correct_decision TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_decision_corrections_entity ON decision_corrections(entity_id);
