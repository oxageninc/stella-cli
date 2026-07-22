import re


def slugify(value: str) -> str:
    """Return a lowercase ASCII-ish slug for a display label."""
    lowered = value.strip().lower()
    return re.sub(r"[^a-z0-9]+", "-", lowered)
