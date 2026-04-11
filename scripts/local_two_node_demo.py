#!/usr/bin/env python3
"""Overview: boot a two-node local wobble network and exercise the main happy
path end to end.

Steps:
1. initialize Alice and miner node homes
2. start Alice normally and start miner with integrated mining enabled
3. bootstrap Alice only so miner must discover that chain through peer sync
4. wait for miner to catch up to Alice's funded chain
5. submit a payment from Alice to miner
6. wait for miner to mine the payment and for both nodes to converge
7. leave command and server logs in one run directory for inspection
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class NodePorts:
    listen: str
    admin: str


ROOT_DIR = Path(__file__).resolve().parent.parent
RUN_DIR = Path(os.environ.get("RUN_DIR") or tempfile.mkdtemp(prefix="wobble-demo."))
NODE_A_HOME = RUN_DIR / "alice"
NODE_B_HOME = RUN_DIR / "miner"
NODE_A_LOG = RUN_DIR / "alice-serve.log"
NODE_B_LOG = RUN_DIR / "miner-serve.log"
COMMAND_LOG = RUN_DIR / "commands.log"
BIN = ROOT_DIR / "target" / "debug" / "wobble"
PAYMENT_AMOUNT = int(os.environ.get("PAYMENT_AMOUNT", "30"))
BOOTSTRAP_BLOCKS = int(os.environ.get("BOOTSTRAP_BLOCKS", "2"))
CONFIRM_TIMEOUT_SECONDS = float(os.environ.get("CONFIRM_TIMEOUT_SECONDS", "20"))
POLL_INTERVAL_SECONDS = float(os.environ.get("POLL_INTERVAL_SECONDS", "0.5"))
COMMAND_TIMEOUT_SECONDS = float(os.environ.get("COMMAND_TIMEOUT_SECONDS", "5"))
STARTUP_PROBE_TIMEOUT_SECONDS = float(os.environ.get("STARTUP_PROBE_TIMEOUT_SECONDS", "1.5"))
RUST_LOG = os.environ.get("RUST_LOG", "wobble=info")


def log(message: str) -> None:
    """Writes one timestamped line to stdout and the command log."""
    line = f"[{time.strftime('%H:%M:%S')}] {message}"
    print(line, flush=True)
    with COMMAND_LOG.open("a", encoding="utf-8") as handle:
        handle.write(f"{line}\n")


def append_output(text: str) -> None:
    """Appends raw command output to the command log and mirrors it to stdout."""
    if not text:
        return
    if isinstance(text, bytes):
        text = text.decode("utf-8", errors="replace")
    print(text, end="" if text.endswith("\n") else "\n", flush=True)
    with COMMAND_LOG.open("a", encoding="utf-8") as handle:
        handle.write(text)
        if not text.endswith("\n"):
            handle.write("\n")


def run_command(
    *args: str,
    check: bool = True,
    timeout: float | None = None,
    quiet: bool = False,
) -> subprocess.CompletedProcess[str]:
    """Runs one wobble-facing command, logs it, and returns captured text."""
    if not quiet:
        log(f"RUN {' '.join(args)}")
    try:
        result = subprocess.run(
            args,
            cwd=ROOT_DIR,
            capture_output=True,
            text=True,
            check=False,
            timeout=COMMAND_TIMEOUT_SECONDS if timeout is None else timeout,
        )
    except subprocess.TimeoutExpired as err:
        stdout = err.stdout or ""
        stderr = err.stderr or ""
        append_output(stdout)
        append_output(stderr)
        raise RuntimeError(
            f"command timed out after {err.timeout:.1f}s: {' '.join(args)}"
        ) from err
    if not quiet:
        append_output(result.stdout)
        append_output(result.stderr)
    if check and result.returncode != 0:
        raise RuntimeError(f"command failed with exit code {result.returncode}: {' '.join(args)}")
    return result


def write_json(path: Path, value: object) -> None:
    """Writes one pretty JSON file used by the demo homes."""
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def allocate_local_port() -> int:
    """Finds one currently free localhost TCP port for a demo listener."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        return int(sock.getsockname()[1])


def parse_fields(output: str) -> dict[str, str]:
    """Parses simple `key: value` CLI output into a dictionary."""
    fields: dict[str, str] = {}
    for line in output.splitlines():
        if ": " not in line:
            continue
        key, value = line.split(": ", 1)
        fields[key.strip()] = value.strip()
    return fields


def start_server(home: Path, log_path: Path, mining: bool) -> tuple[subprocess.Popen[str], object]:
    """Starts one local wobble server and captures its combined stdout/stderr."""
    log_handle = log_path.open("w", encoding="utf-8")
    command = [str(BIN), "serve", "--home", str(home)]
    if mining:
        command.append("--mining")
    log(f"starting {'miner' if mining else 'alice'} server")
    process = subprocess.Popen(
        command,
        cwd=ROOT_DIR,
        stdout=log_handle,
        stderr=subprocess.STDOUT,
        text=True,
        env={**os.environ, "RUST_LOG": RUST_LOG},
    )
    return process, log_handle


def wait_for_running_server(home: Path, name: str, process: subprocess.Popen[str]) -> None:
    """Polls `wobble status` until the local admin server reports itself live."""
    deadline = time.time() + 10
    while time.time() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"{name} server exited early with code {process.returncode}")
        try:
            result = run_command(
                str(BIN),
                "status",
                "--home",
                str(home),
                check=False,
                timeout=STARTUP_PROBE_TIMEOUT_SECONDS,
                quiet=True,
            )
        except RuntimeError as err:
            log(f"{name} status probe not ready yet ({err})")
            time.sleep(0.2)
            continue
        if result.returncode == 0 and "server: running" in result.stdout:
            log(f"{name} server is running")
            return
        time.sleep(0.2)
    raise RuntimeError(f"{name} server did not become ready in time")


def wallet_info(home: Path) -> dict[str, str]:
    """Fetches wallet-info output and returns its parsed fields."""
    result = run_command(str(BIN), "wallet-info", "--home", str(home), quiet=True)
    return parse_fields(result.stdout)


def status_info(home: Path) -> dict[str, str]:
    """Fetches status output and returns its parsed fields."""
    result = run_command(str(BIN), "status", "--home", str(home), quiet=True)
    return parse_fields(result.stdout)


def wait_for_confirmation(sender_home: Path, recipient_home: Path) -> None:
    """Polls local state until the payment has been mined and relayed to both nodes."""
    expected_sender_balance = BOOTSTRAP_BLOCKS * 50 - PAYMENT_AMOUNT
    expected_recipient_balance = PAYMENT_AMOUNT + 50
    expected_height = BOOTSTRAP_BLOCKS

    deadline = time.time() + CONFIRM_TIMEOUT_SECONDS
    while time.time() < deadline:
        sender_wallet = wallet_info(sender_home)
        recipient_wallet = wallet_info(recipient_home)
        sender_status = status_info(sender_home)
        recipient_status = status_info(recipient_home)
        sender_balance = int(sender_wallet["balance"])
        recipient_balance = int(recipient_wallet["balance"])
        sender_height = int(sender_status["height"])
        recipient_height = int(recipient_status["height"])

        log(
            "poll sender_balance="
            f"{sender_balance} recipient_balance={recipient_balance} "
            f"sender_height={sender_height} recipient_height={recipient_height}"
        )

        if (
            sender_balance == expected_sender_balance
            and recipient_balance == expected_recipient_balance
            and sender_height == expected_height
            and recipient_height == expected_height
        ):
            log("payment confirmed on chain")
            return

        time.sleep(POLL_INTERVAL_SECONDS)

    raise RuntimeError("timed out waiting for the submitted payment to confirm")


def wait_for_tip_sync(source_home: Path, target_home: Path, target_name: str) -> None:
    """Polls until `target_home` reports the same persisted tip and height as `source_home`."""
    deadline = time.time() + CONFIRM_TIMEOUT_SECONDS
    while time.time() < deadline:
        source_status = status_info(source_home)
        target_status = status_info(target_home)
        source_tip = source_status["best tip"]
        target_tip = target_status["best tip"]
        source_height = source_status["height"]
        target_height = target_status["height"]

        log(
            f"sync source_height={source_height} source_tip={source_tip} "
            f"{target_name}_height={target_height} {target_name}_tip={target_tip}"
        )

        if source_tip != "<none>" and source_tip == target_tip and source_height == target_height:
            log(f"{target_name} caught up to the bootstrapped chain")
            return

        time.sleep(POLL_INTERVAL_SECONDS)

    raise RuntimeError(f"timed out waiting for {target_name} to catch up to the bootstrapped chain")


def terminate_process(process: subprocess.Popen[str] | None, name: str) -> None:
    """Stops one child process without failing cleanup on already-exited children."""
    if process is None:
        return
    if process.poll() is not None:
        return
    log(f"stopping {name} server")
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        log(f"{name} server did not exit on SIGTERM; sending SIGKILL")
        process.kill()
        process.wait(timeout=5)


def configure_home(home: Path, ports: NodePorts, node_name: str, peers: list[dict[str, str]]) -> None:
    """Rewrites one initialized node home with demo-specific config and peers."""
    config = {
        "listen_addr": ports.listen,
        "admin_addr": ports.admin,
        "network": "wobble-local",
        "node_name": node_name,
        "mining": {
            "enabled": False,
            "reward_wallet": None,
            "interval_ms": 250,
            "max_transactions": 100,
            "bits": "0x207fffff",
        },
    }
    write_json(home / "config.json", config)
    write_json(home / "peers.json", peers)


def main() -> int:
    """Runs the local two-node funding, payment, and confirmation demo."""
    alice_process: subprocess.Popen[str] | None = None
    miner_process: subprocess.Popen[str] | None = None
    alice_log_handle = None
    miner_log_handle = None
    try:
        log(f"run dir: {RUN_DIR}")
        log("building wobble binary")
        build = subprocess.run(
            ["cargo", "build"],
            cwd=ROOT_DIR,
            check=False,
            text=True,
            capture_output=True,
        )
        append_output(build.stdout)
        append_output(build.stderr)
        if build.returncode != 0:
            raise RuntimeError(f"cargo build failed with exit code {build.returncode}")

        NODE_A_HOME.mkdir(parents=True, exist_ok=True)
        NODE_B_HOME.mkdir(parents=True, exist_ok=True)

        run_command(str(BIN), "init", "--home", str(NODE_A_HOME))
        run_command(str(BIN), "init", "--home", str(NODE_B_HOME))

        alice_peer_port = allocate_local_port()
        miner_peer_port = allocate_local_port()
        alice_admin_port = allocate_local_port()
        miner_admin_port = allocate_local_port()
        alice_ports = NodePorts(
            listen=f"127.0.0.1:{alice_peer_port}",
            admin=f"127.0.0.1:{alice_admin_port}",
        )
        miner_ports = NodePorts(
            listen=f"127.0.0.1:{miner_peer_port}",
            admin=f"127.0.0.1:{miner_admin_port}",
        )

        configure_home(
            NODE_A_HOME,
            alice_ports,
            "alice",
            [{"addr": miner_ports.listen, "node_name": "miner"}],
        )
        configure_home(
            NODE_B_HOME,
            miner_ports,
            "miner",
            [{"addr": alice_ports.listen, "node_name": "alice"}],
        )

        log(f"alice listen addr: {alice_ports.listen}")
        log(f"alice admin addr: {alice_ports.admin}")
        log(f"miner listen addr: {miner_ports.listen}")
        log(f"miner admin addr: {miner_ports.admin}")

        alice_process, alice_log_handle = start_server(NODE_A_HOME, NODE_A_LOG, mining=False)
        miner_process, miner_log_handle = start_server(NODE_B_HOME, NODE_B_LOG, mining=True)

        wait_for_running_server(NODE_A_HOME, "alice", alice_process)
        wait_for_running_server(NODE_B_HOME, "miner", miner_process)

        alice_wallet = wallet_info(NODE_A_HOME)
        miner_wallet = wallet_info(NODE_B_HOME)
        miner_public_key = miner_wallet["public key"]

        log(f"alice public key: {alice_wallet['public key']}")
        log(f"miner public key: {miner_public_key}")

        run_command(str(BIN), "bootstrap", "--home", str(NODE_A_HOME), "--blocks", str(BOOTSTRAP_BLOCKS))
        wait_for_tip_sync(NODE_A_HOME, NODE_B_HOME, "miner")

        run_command(
            str(BIN),
            "submit-payment",
            "--home",
            str(NODE_A_HOME),
            miner_public_key,
            str(PAYMENT_AMOUNT),
        )

        wait_for_confirmation(NODE_A_HOME, NODE_B_HOME)
        alice_status = status_info(NODE_A_HOME)
        miner_status = status_info(NODE_B_HOME)
        alice_wallet = wallet_info(NODE_A_HOME)
        miner_wallet = wallet_info(NODE_B_HOME)

        log(
            "final alice "
            f"height={alice_status['height']} balance={alice_wallet['balance']} "
            f"best_tip={alice_status['best tip']}"
        )
        log(
            "final miner "
            f"height={miner_status['height']} balance={miner_wallet['balance']} "
            f"best_tip={miner_status['best tip']}"
        )

        log(f"alice server log: {NODE_A_LOG}")
        log(f"miner server log: {NODE_B_LOG}")
        log(f"command log: {COMMAND_LOG}")
        log("demo completed successfully")
        return 0
    except Exception as err:
        log(f"demo failed: {err}")
        log(f"demo failed; artifacts left in {RUN_DIR}")
        return 1
    finally:
        terminate_process(alice_process, "alice")
        terminate_process(miner_process, "miner")
        if alice_log_handle is not None:
            alice_log_handle.close()
        if miner_log_handle is not None:
            miner_log_handle.close()


if __name__ == "__main__":
    sys.exit(main())
