// config.js - shared constants and solver strategy for the sim server.
// Ports of the module-level constants in server/ms_server.py.

export const DIFFS = ["beginner", "intermediate", "expert"];

// server/ms_server.py SOLVER_STRATEGY verbatim.
export const SOLVER_STRATEGY = new Map([
  ["tiebreak", "info"],
  ["first", "center"],
  ["use_chord", true],
  ["refresh", false],
]);

// Estimated CPU per simulated game (production box figures, unchanged).
export const GAME_CPU_SECONDS = {
  beginner: 0.002,
  intermediate: 0.016,
  expert: 0.076,
};
export const HEAVY_CPU_SECONDS = 0.25;

export const NONCE_TTL = 60;
export const MAX_AUTH_FAILS = 5;
export const MAX_LINE = 65536;
export const LB_WINDOW = 60.0;
export const LB_MAX = 20;
export const LB_MAX_IPS = 4096;
export const NAME_RE = /^[A-Za-z0-9_-]{1,16}$/;
