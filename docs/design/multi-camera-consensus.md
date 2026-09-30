# RFP multi-camera contract

RFP selects cameras and requests strict hub-owned 2-of-2 consensus for rep
workouts. It never adds per-camera counts. API 1.6 progress preserves session
identity and consensus diagnostics; events precede credited progress. The hub
pins movement/model identity and settings. Legacy single-camera behavior remains
available. Continuous activities retain single-camera processing.

Camera settings apply to the next workout. Multi-view workouts require an active
qualified movement, not the legacy unversioned exercise fallback. No automatic
single-camera credit when quorum is unavailable. Existing explicit honor mode
remains distinct from camera evidence. Accuracy qualification must compare the
same reviewed sessions against the single-camera baseline.
