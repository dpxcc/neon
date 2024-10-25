from __future__ import annotations

import enum
import os
from enum import StrEnum

import pytest
from typing_extensions import override

"""
This fixture is used to determine which version of Postgres to use for tests.
"""


# Inherit PgVersion from str rather than int to make it easier to pass as a command-line argument
@enum.unique
class PgVersion(StrEnum):
    V14 = "14"
    V15 = "15"
    V16 = "16"
    V17 = "17"
    # Instead of making version an optional parameter in methods, we can use this fake entry
    # to explicitly rely on the default server version (could be different from pg_version fixture value)
    NOT_SET = "<-POSTRGRES VERSION IS NOT SET->"

    # Make it less confusing in logs
    @override
    def __repr__(self) -> str:
        return f"'{self.value}'"

    @override
    def __str__(self) -> str:
        return self.value

    # In GitHub workflows we use Postgres version with v-prefix (e.g. v14 instead of just 14),
    # sometime we need to do so in tests.
    @property
    def v_prefixed(self) -> str:
        return f"v{self.value}"

    @classmethod
    @override
    def _missing_(cls, value: object) -> PgVersion | None:
        if not isinstance(value, str):
            return None

        known_values = {v.value for _, v in cls.__members__.items()}

        # Allow passing version as v-prefixed string (e.g. "v14")
        if value.lower().startswith("v") and (v := value[1:]) in known_values:
            return cls(v)
        # Allow passing version as an int (i.e. both "15" and "150002" matches PgVersion.V15)
        elif value.isdigit() and (v := value[:2]) in known_values:
            return cls(v)
        else:
            return None


DEFAULT_VERSION: PgVersion = PgVersion.V16


def skip_on_postgres(version: PgVersion, reason: str):
    return pytest.mark.skipif(
        PgVersion(os.environ.get("DEFAULT_PG_VERSION", DEFAULT_VERSION)) is version,
        reason=reason,
    )


def xfail_on_postgres(version: PgVersion, reason: str):
    return pytest.mark.xfail(
        PgVersion(os.environ.get("DEFAULT_PG_VERSION", DEFAULT_VERSION)) is version,
        reason=reason,
    )


def run_only_on_default_postgres(reason: str):
    return pytest.mark.skipif(
        PgVersion(os.environ.get("DEFAULT_PG_VERSION", DEFAULT_VERSION)) is not DEFAULT_VERSION,
        reason=reason,
    )
