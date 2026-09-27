-- Development/integration only: the projector identity (ADR-0021). Its privileges are granted
-- by `ledger-admin migrate --projector-role ledger_projector` (migration 0011), never here.
CREATE ROLE ledger_projector LOGIN PASSWORD 'ledger-projector-development-only';
