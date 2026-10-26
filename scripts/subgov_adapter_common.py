"""Credential-free, restrictive state persistence for command adapters."""
import fcntl
import json
import os
import tempfile
from contextlib import contextmanager
from pathlib import Path


def describe_error(error, stderr=None):
    """Name an exception's cause, including a captured subprocess stderr.

    Logging only the class hid the 2026-10-23 Z.AI replica-count mismatch
    (CalledProcessError) and the Codex missing-demand-block failure (ValueError)
    for days (subgov-28a476df). CalledProcessError and TimeoutExpired carry the
    child's stderr without showing it, so append it, whitespace-flattened and
    tail-truncated, to keep the journal line bounded and single-line. A caller
    holding a completed result's stderr can pass it directly.
    """
    text = str(error).strip() or type(error).__name__
    if stderr is None:
        stderr = getattr(error, "stderr", None)
    if isinstance(stderr, bytes):
        stderr = stderr.decode("utf-8", "replace")
    if stderr and stderr.strip():
        text += "; subprocess stderr: " + " ".join(stderr.split())[-1000:]
    return text


def read_json(path, default=None):
    try:
        with open(path, encoding="utf-8") as stream:
            return json.load(stream)
    except FileNotFoundError:
        if default is not None:
            return default
        raise


def atomic_json(path, value):
    atomic_text(path, json.dumps(value, sort_keys=True, allow_nan=False) + "\n")


def atomic_text(path, value):
    directory = os.path.dirname(os.path.abspath(path))
    os.makedirs(directory, mode=0o700, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".subgov-", dir=directory)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            os.fchmod(stream.fileno(), 0o600)
            stream.write(value)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory_fd = os.open(directory, os.O_RDONLY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


@contextmanager
def locked(path):
    os.makedirs(os.path.dirname(os.path.abspath(path)), mode=0o700, exist_ok=True)
    fd = os.open(path + ".lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        os.fchmod(fd, 0o600)
        # Never spend a source's 15-second budget waiting for another collector.
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        yield
    finally:
        os.close(fd)


def live_children(task, proc_root="/proc"):
    """Return the child PIDs of a /proc/<pid>/task/<tid> entry that can still act.

    A zombie (state Z) or dead (X) child has already exited and cannot run
    work, so it must not block retiring an idle supervisor: a supervisor that
    never reaps one of its children would otherwise be unretirable forever.
    A child whose state cannot be read is treated as live, which keeps the
    check conservative (the worker is simply not stopped this cycle).
    """
    live = []
    for child in (Path(task) / "children").read_text().split():
        try:
            stat = (Path(proc_root) / child / "stat").read_text()
            state = stat.rsplit(")", 1)[1].split()[0]
        except (OSError, IndexError):
            live.append(child)
            continue
        if state not in ("Z", "X"):
            live.append(child)
    return live
