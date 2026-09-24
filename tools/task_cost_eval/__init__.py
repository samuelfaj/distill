"""Small, fail-closed evaluator for paired task-cost recordings."""

from .evaluate import build_report, load_cohort, load_runs

__all__ = ["build_report", "load_cohort", "load_runs"]
