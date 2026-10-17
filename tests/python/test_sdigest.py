import io
import subprocess

import pytest

from sdigest import may_equal, sdigest

BLOCK = 64 * 1024


@pytest.fixture
def data():
    return bytes(range(256)) * (40 * BLOCK // 256) + b"tail"


def test_path_and_file_objects_agree(tmp_path, data):
    p = tmp_path / "f"
    p.write_bytes(data)
    expected = sdigest(str(p), coverage=5)
    assert expected.startswith(f"sd1:{len(data)}:5:")
    assert sdigest(p, coverage=5) == expected
    assert sdigest(io.BytesIO(data), coverage=5) == expected
    with open(p, "rb") as f:
        assert sdigest(f, coverage=5) == expected


def test_matches_cli(tmp_path, data):
    p = tmp_path / "f"
    p.write_bytes(data)
    cli = subprocess.run(
        ["cargo", "run", "-q", "--bin", "sdigest", "--", "-c", "2.5", str(p)],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    assert cli.split("  ")[0] == sdigest(p, coverage=2.5)


def test_default_coverage(data):
    assert sdigest(io.BytesIO(data)).split(":")[2] == "1"


def test_levels(data):
    assert sdigest(io.BytesIO(data), 5) == sdigest(io.BytesIO(data), coverage=1)
    assert sdigest(io.BytesIO(data), level=0).split(":")[2] == "0"
    with pytest.raises(ValueError):
        sdigest(io.BytesIO(data), 3, coverage=1)
    with pytest.raises(ValueError):
        sdigest(io.BytesIO(data), 10)


def test_may_equal(data):
    changed = b"X" + data[1:]
    a = sdigest(io.BytesIO(data), coverage=50)
    assert may_equal(a, sdigest(io.BytesIO(data), coverage=50)) is True
    assert may_equal(a, sdigest(io.BytesIO(changed), coverage=50)) is False
    assert may_equal(a, sdigest(io.BytesIO(data), coverage=10)) is None


def test_errors(tmp_path):
    with pytest.raises(ValueError):
        sdigest(io.BytesIO(b""), coverage=101)
    with pytest.raises(FileNotFoundError):
        sdigest(tmp_path / "missing")
    with pytest.raises(TypeError):
        sdigest(io.StringIO("text"))
    with pytest.raises(ValueError):
        may_equal("nope", "nope")


def test_python_exceptions_propagate():
    class Boom(io.BytesIO):
        def read(self, n=-1):
            raise RuntimeError("boom")

    with pytest.raises(RuntimeError, match="boom"):
        sdigest(Boom(b"x"))
