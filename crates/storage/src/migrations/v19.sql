-- Service tier requested by each attempt, so a fast request can be repriced at
-- fast rates. NULL is standard processing, including rows written before v19.
ALTER TABLE usage_requests ADD COLUMN service_tier TEXT;
