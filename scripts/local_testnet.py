#!/usr/bin/env python3
"""Overview: boot a configurable local wobble testnet under /tmp.

This harness creates one temporary home per node, starts one `wobble serve`
process for each home, and wires the nodes together through `peers.json`.
By default every generated node also points at the local user's node from
`~/.wobble/config.json`, so the temporary cluster joins that existing listener
without mutating the user's home.

When a seed peer is configured, the cluster treats that external node as the
source of truth and waits for the temporary nodes to sync to it. Only isolated
runs with no seed peer bootstrap a fresh local genesis on `node0`.

Optional random payments submit confirmed transfers at a fixed rate. The
harness waits for each payment to clear before choosing the next sender so it
does not intentionally overspend stale confirmed balances.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import socket
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from pathlib import Path


ROOT_DIR = Path(__file__).resolve().parent.parent
BIN = ROOT_DIR / "target" / "debug" / "wobble"
DEFAULT_HOME = Path.home() / ".wobble"
RUST_LOG = os.environ.get("RUST_LOG", "wobble=info")


@dataclass(frozen=True)
class NodePorts:
    listen: str
    admin: str


@dataclass
class NodeHandle:
    name: str
    home: Path
    ports: NodePorts
    log_path: Path
    process: subprocess.Popen[str] | None = None
    log_handle: object | None = None
    tee_thread: threading.Thread | None = None


def parse_args() -> argparse.Namespace:
    """Parses script arguments for one local configurable testnet run."""
    parser = argparse.ArgumentParser(description="boot a configurable local wobble testnet")
    parser.add_argument("--nodes", type=int, default=3, help="number of temporary nodes to start")
    parser.add_argument("--mining-node", type=int, default=0, help="index of the integrated miner node")
    parser.add_argument("--bootstrap-blocks", type=int, default=2, help="coinbase blocks to mine on node0 before payments")
    parser.add_argument("--network", default="wobble-local", help="network name for the temporary cluster")
    parser.add_argument("--seed-peer", default=None, help="existing peer listener to include in every peers.json")
    parser.add_argument("--payment-count", type=int, default=0, help="number of random confirmed payments to submit after bootstrap")
    parser.add_argument("--payment-rate", type=float, default=0.0, help="payments per second when random payments are enabled")
    parser.add_argument("--payment-amount", type=int, default=5, help="coins sent by each random payment")
    parser.add_argument("--tee-logs", action="store_true", help="mirror each server log to stderr with a node prefix")
    parser.add_argument("--run-dir", type=Path, default=None, help="existing directory to reuse instead of a fresh /tmp run")
    return parser.parse_args()


class LocalTestNet:
    """Owns one temporary multi-node local wobble testnet run."""

    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.run_dir = args.run_dir or Path(tempfile.mkdtemp(prefix="wobble-testnet."))
        self.command_log = self.run_dir / "commands.log"
        self.randomizer = random.Random()
        self.nodes: list[NodeHandle] = []
        normalized_seed_peer = self.normalize_seed_peer(args.seed_peer)
        if args.seed_peer is not None:
            self.seed_peer = normalized_seed_peer
        else:
            self.seed_peer = self.default_seed_peer()

    def normalize_seed_peer(self, value: str | None) -> str | None:
        """Treats empty or sentinel CLI values as an absent seed peer."""
        if value is None:
            return None
        normalized = value.strip()
        if not normalized:
            return None
        if normalized.lower() in {"none", "null"}:
            return None
        return normalized

    def log(self, message: str) -> None:
        """Writes one timestamped line to stdout and the shared command log."""
        line = f"[{time.strftime('%H:%M:%S')}] {message}"
        print(line, flush=True)
        with self.command_log.open("a", encoding="utf-8") as handle:
            handle.write(f"{line}\n")

    def append_output(self, text: str) -> None:
        """Appends raw command output to the command log and mirrors it to stdout."""
        if not text:
            return
        if isinstance(text, bytes):
            text = text.decode("utf-8", errors="replace")
        print(text, end="" if text.endswith("\n") else "\n", flush=True)
        with self.command_log.open("a", encoding="utf-8") as handle:
            handle.write(text)
            if not text.endswith("\n"):
                handle.write("\n")

    def tee_server_output(self, node: NodeHandle) -> None:
        """Mirrors one server process's combined output to stderr with a node prefix."""
        assert node.process is not None
        assert node.process.stdout is not None
        assert node.log_handle is not None
        for line in node.process.stdout:
            node.log_handle.write(line)
            node.log_handle.flush()
            rendered = line.rstrip("\n")
            print(f"[{node.name}] {rendered}", file=sys.stderr, flush=True)

    def run_command(
        self,
        *args: str,
        check: bool = True,
        timeout: float = 5.0,
        quiet: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        """Runs one command, logs it, and returns captured text."""
        if not quiet:
            self.log(f"RUN {' '.join(args)}")
        try:
            result = subprocess.run(
                args,
                cwd=ROOT_DIR,
                capture_output=True,
                text=True,
                check=False,
                timeout=timeout,
            )
        except subprocess.TimeoutExpired as err:
            stdout = err.stdout or ""
            stderr = err.stderr or ""
            self.append_output(stdout)
            self.append_output(stderr)
            raise RuntimeError(
                f"command timed out after {err.timeout:.1f}s: {' '.join(args)}"
            ) from err
        if not quiet:
            self.append_output(result.stdout)
            self.append_output(result.stderr)
        if check and result.returncode != 0:
            raise RuntimeError(
                f"command failed with exit code {result.returncode}: {' '.join(args)}"
            )
        return result

    def build_binary(self) -> None:
        """Builds the wobble binary used by all managed node processes."""
        self.log("building wobble binary")
        build = subprocess.run(
            ["cargo", "build"],
            cwd=ROOT_DIR,
            check=False,
            text=True,
            capture_output=True,
        )
        self.append_output(build.stdout)
        self.append_output(build.stderr)
        if build.returncode != 0:
            raise RuntimeError(f"cargo build failed with exit code {build.returncode}")

    def allocate_local_port(self) -> int:
        """Finds one currently free localhost TCP port for a listener."""
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.bind(("127.0.0.1", 0))
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            return int(sock.getsockname()[1])

    def parse_fields(self, output: str) -> dict[str, str]:
        """Parses simple `key: value` CLI output into a dictionary."""
        fields: dict[str, str] = {}
        for line in output.splitlines():
            if ": " not in line:
                continue
            key, value = line.split(": ", 1)
            fields[key.strip()] = value.strip()
        return fields

    def write_json(self, path: Path, value: object) -> None:
        """Writes one pretty JSON file to disk."""
        path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")

    def default_seed_peer(self) -> str | None:
        """Returns the local user's listen address when `~/.wobble` exists."""
        config_path = DEFAULT_HOME / "config.json"
        if not config_path.exists():
            return None
        try:
            config = json.loads(config_path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            return None
        listen_addr = config.get("listen_addr")
        return listen_addr if isinstance(listen_addr, str) else None

    def status_info_for_home(self, home: Path) -> dict[str, str]:
        """Returns parsed `status` fields for an arbitrary node home."""
        result = self.run_command(
            str(BIN),
            "status",
            "--home",
            str(home),
            check=False,
            quiet=True,
            timeout=30.0,
        )
        if result.returncode != 0:
            stderr = (result.stderr or "").strip()
            stdout = (result.stdout or "").strip()
            details = stderr or stdout or "status command returned no output"
            raise RuntimeError(f"status probe failed for {home}: {details}")
        return self.parse_fields(result.stdout)

    def create_nodes(self) -> None:
        """Allocates one temporary home, ports, and log file per local node."""
        for index in range(self.args.nodes):
            home = self.run_dir / f"node{index}"
            home.mkdir(parents=True, exist_ok=True)
            self.nodes.append(
                NodeHandle(
                    name=f"node{index}",
                    home=home,
                    ports=NodePorts(
                        listen=f"127.0.0.1:{self.allocate_local_port()}",
                        admin=f"127.0.0.1:{self.allocate_local_port()}",
                    ),
                    log_path=self.run_dir / f"node{index}-serve.log",
                )
            )

    def init_homes(self) -> None:
        """Initializes every managed node home through `wobble init`."""
        for node in self.nodes:
            self.run_command(str(BIN), "init", "--home", str(node.home))

    def peer_specs_for(self, index: int) -> list[dict[str, str | None]]:
        """Returns the configured peers for one managed node."""
        if self.seed_peer:
            if index == 0:
                return [{"addr": self.seed_peer, "node_name": None}]
            return [{"addr": self.nodes[0].ports.listen, "node_name": self.nodes[0].name}]
        peers: list[dict[str, str | None]] = []
        for peer_index, peer in enumerate(self.nodes):
            if peer_index == index:
                continue
            peers.append({"addr": peer.ports.listen, "node_name": peer.name})
        if self.seed_peer:
            peers.append({"addr": self.seed_peer, "node_name": None})
        return peers

    def configure_homes(self) -> None:
        """Writes config and peers files for every managed node home."""
        for index, node in enumerate(self.nodes):
            config = {
                "listen_addr": node.ports.listen,
                "admin_addr": node.ports.admin,
                "network": self.args.network,
                "node_name": node.name,
                "mining": {
                    "enabled": index == self.args.mining_node,
                    "reward_wallet": None,
                    "interval_ms": 250,
                    "max_transactions": 100,
                    "bits": "0x207fffff",
                },
            }
            self.write_json(node.home / "config.json", config)
            self.write_json(node.home / "peers.json", self.peer_specs_for(index))
            self.log(f"{node.name} listen={node.ports.listen} admin={node.ports.admin}")

    def start_node(self, node: NodeHandle, mining: bool) -> None:
        """Starts one local wobble server and captures its combined stdout/stderr."""
        command = [str(BIN), "serve", "--home", str(node.home)]
        if mining:
            command.append("--mining")
        self.log(f"starting {node.name} server")
        if self.args.tee_logs:
            node.log_handle = node.log_path.open("w", encoding="utf-8")
            node.process = subprocess.Popen(
                command,
                cwd=ROOT_DIR,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                bufsize=1,
                env={**os.environ, "RUST_LOG": RUST_LOG},
            )
            tee = threading.Thread(
                target=self.tee_server_output,
                args=(node,),
                name=f"{node.name}-log-tee",
                daemon=True,
            )
            tee.start()
            node.tee_thread = tee
        else:
            node.log_handle = node.log_path.open("w", encoding="utf-8")
            node.process = subprocess.Popen(
                command,
                cwd=ROOT_DIR,
                stdout=node.log_handle,
                stderr=subprocess.STDOUT,
                text=True,
                env={**os.environ, "RUST_LOG": RUST_LOG},
            )

    def start_servers(self) -> None:
        """Starts all managed node servers and waits for them to report live."""
        if self.seed_peer:
            self.start_seeded_servers()
            return
        for index, node in enumerate(self.nodes):
            self.start_node(node, mining=index == self.args.mining_node)
        for node in self.nodes:
            self.wait_for_running_server(node)

    def start_seeded_servers(self) -> None:
        """Starts a seeded cluster by syncing node0 first, then local followers."""
        seed_follower = self.nodes[0]
        self.start_node(seed_follower, mining=self.args.mining_node == 0)
        self.wait_for_running_server(seed_follower)

        for index, node in enumerate(self.nodes[1:], start=1):
            self.start_node(node, mining=index == self.args.mining_node)
        for node in self.nodes[1:]:
            self.wait_for_running_server(node)

    def wait_for_first_tip(self, node: NodeHandle, source: str | None) -> None:
        """Polls until one node reports its first tip from bootstrap or seed sync."""
        deadline = time.time() + 30
        next_progress_log = time.time()
        while time.time() < deadline:
            self.ensure_nodes_running()
            status = self.status_info(node)
            tip, height = self.effective_tip_fields(status)
            if tip != "<none>":
                self.log(
                    f"{node.name} synced first tip from {source or 'bootstrap'} "
                    f"height={height} tip={tip}"
                )
                return
            if time.time() >= next_progress_log:
                self.log(f"waiting for {node.name} to report its first tip")
                next_progress_log = time.time() + 10
            time.sleep(0.5)
        raise RuntimeError(
            f"{node.name} did not report a tip while syncing from {source or 'bootstrap'}"
        )

    def wait_for_running_server(self, node: NodeHandle) -> None:
        """Polls `wobble status` until the local admin server reports itself live."""
        deadline = time.time() + 10
        while time.time() < deadline:
            assert node.process is not None
            if node.process.poll() is not None:
                raise RuntimeError(f"{node.name} server exited early with code {node.process.returncode}")
            result = self.run_command(
                str(BIN),
                "status",
                "--home",
                str(node.home),
                check=False,
                timeout=30.0,
                quiet=True,
            )
            if result.returncode == 0 and "server: running" in result.stdout:
                self.log(f"{node.name} server is running")
                return
            time.sleep(0.2)
        raise RuntimeError(f"{node.name} server did not become ready in time")

    def wallet_info(self, node: NodeHandle) -> dict[str, str]:
        """Returns parsed `wallet info` fields for one node home."""
        result = self.run_command(
            str(BIN),
            "wallet",
            "info",
            "--home",
            str(node.home),
            quiet=True,
            timeout=30.0,
        )
        return self.parse_fields(result.stdout)

    def balance_info(self, node: NodeHandle) -> dict[str, str]:
        """Returns parsed `balance` fields for one node home."""
        result = self.run_command(
            str(BIN),
            "balance",
            "--home",
            str(node.home),
            quiet=True,
            timeout=30.0,
        )
        return self.parse_fields(result.stdout)

    def status_info(self, node: NodeHandle) -> dict[str, str]:
        """Returns parsed `status` fields for one node home."""
        result = self.run_command(
            str(BIN),
            "status",
            "--home",
            str(node.home),
            check=False,
            quiet=True,
            timeout=30.0,
        )
        if result.returncode != 0:
            stderr = (result.stderr or "").strip()
            stdout = (result.stdout or "").strip()
            details = stderr or stdout or "status command returned no output"
            raise RuntimeError(f"status probe failed for {node.name}: {details}")
        return self.parse_fields(result.stdout)

    def effective_tip_fields(self, status: dict[str, str]) -> tuple[str, str]:
        """Returns the best available tip and height from one status snapshot."""
        if status.get("server") == "running":
            live_tip = status.get("live best tip", "<none>")
            live_height = status.get("live height", "<none>")
            if live_tip != "<none>":
                return live_tip, live_height
        return status.get("best tip", "<none>"), status.get("height", "<none>")

    def seed_tip_info(self) -> dict[str, str]:
        """Returns the seeded peer's advertised tip summary."""
        assert self.seed_peer is not None
        result = self.run_command(
            str(BIN),
            "inspect",
            "tip",
            self.seed_peer,
            self.args.network,
            quiet=True,
            timeout=30.0,
        )
        return self.parse_fields(result.stdout)

    def ensure_seed_running(self) -> None:
        """Validates that the default local seed server is live before waiting on it."""
        if self.seed_peer != self.default_seed_peer():
            return
        status = self.status_info_for_home(DEFAULT_HOME)
        if status.get("server") != "running":
            raise RuntimeError(
                f"seed home {DEFAULT_HOME} is not running; start `wobble serve` first or disable seeding"
            )
        if status.get("live best tip") == "<none>":
            raise RuntimeError(
                f"seed home {DEFAULT_HOME} is running but has no live best tip; bootstrap that node first or disable seeding"
            )

    def ensure_nodes_running(self) -> None:
        """Raises if any managed server process has exited unexpectedly."""
        for node in self.nodes:
            if node.process is not None and node.process.poll() is not None:
                raise RuntimeError(
                    f"{node.name} server exited early with code {node.process.returncode}"
                )

    def bootstrap(self) -> None:
        """Either bootstraps a fresh local chain or joins the configured seed chain."""
        if self.seed_peer:
            self.ensure_seed_running()
            seed_tip = self.seed_tip_info()
            if seed_tip.get("peer tip") == "<none>":
                raise RuntimeError(
                    f"seed peer {self.seed_peer} has no best tip; bootstrap that node first or disable seeding"
                )
            self.log(
                f"joining seeded chain from {self.seed_peer} "
                f"tip={seed_tip.get('peer tip')} height={seed_tip.get('peer height')}"
            )
            self.wait_for_cluster_sync(self.nodes[0])
            return

        self.log(f"bootstrapping local chain on {self.nodes[0].name}")
        self.run_command(
            str(BIN),
            "bootstrap",
            "--home",
            str(self.nodes[0].home),
            "--blocks",
            str(self.args.bootstrap_blocks),
            timeout=30.0,
        )
        self.wait_for_cluster_sync(self.nodes[0])

    def wait_for_cluster_sync(self, reference: NodeHandle) -> None:
        """Polls until every node reports the same best available tip and height."""
        next_progress_log = time.time()
        missing_tip_deadline = time.time() + 30 if self.seed_peer else None
        while True:
            self.ensure_nodes_running()
            reference_status = self.status_info(reference)
            reference_tip, reference_height = self.effective_tip_fields(reference_status)
            if reference_tip == "<none>":
                if missing_tip_deadline is not None and time.time() >= missing_tip_deadline:
                    raise RuntimeError(
                        f"{reference.name} did not report a tip while syncing from seed {self.seed_peer}"
                    )
                if time.time() >= next_progress_log:
                    self.log("waiting for reference node to report a tip")
                    next_progress_log = time.time() + 10
                time.sleep(0.5)
                continue

            all_match = True
            mismatches: list[str] = []
            for node in self.nodes:
                node_status = self.status_info(node)
                node_tip, node_height = self.effective_tip_fields(node_status)
                if (
                    node_tip != reference_tip
                    or node_height != reference_height
                ):
                    all_match = False
                    mismatches.append(
                        f"{node.name}(height={node_height} tip={node_tip})"
                    )
            if all_match:
                self.log(f"cluster synced at height={reference_height} best_tip={reference_tip}")
                return
            if time.time() >= next_progress_log:
                if mismatches:
                    self.log(
                        "still waiting for cluster sync: "
                        f"reference height={reference_height} tip={reference_tip} "
                        + " ".join(mismatches[:3])
                    )
                else:
                    self.log(
                        "still waiting for cluster sync: "
                        f"reference height={reference_height} tip={reference_tip}"
                    )
                next_progress_log = time.time() + 10
            time.sleep(0.5)

    def wait_for_empty_mempools(self) -> None:
        """Polls until every node reports an empty mempool."""
        next_progress_log = time.time()
        while True:
            self.ensure_nodes_running()
            mempool_sizes = {
                node.name: self.status_info(node).get("mempool txs", "?") for node in self.nodes
            }
            if all(size == "0" for size in mempool_sizes.values()):
                return
            if time.time() >= next_progress_log:
                self.log(
                    "still waiting for empty mempools: "
                    + " ".join(f"{name}={size}" for name, size in mempool_sizes.items())
                )
                next_progress_log = time.time() + 10
            time.sleep(0.5)

    def wallet_fields(self) -> dict[str, dict[str, str]]:
        """Returns parsed wallet-info output for every managed node."""
        return {node.name: self.wallet_info(node) for node in self.nodes}

    def eligible_senders(self) -> list[NodeHandle]:
        """Returns nodes whose confirmed balance can fund one configured payment."""
        result = []
        for node in self.nodes:
            fields = self.balance_info(node)
            try:
                total = int(fields["total"])
            except (KeyError, ValueError):
                continue
            if total >= self.args.payment_amount:
                result.append(node)
        return result

    def run_random_payments(self) -> None:
        """Submits confirmed random payments across the managed nodes."""
        if self.args.payment_count <= 0:
            return

        wallet_fields = self.wallet_fields()
        interval = 0.0 if self.args.payment_rate <= 0 else 1.0 / self.args.payment_rate
        for payment_index in range(self.args.payment_count):
            senders = self.eligible_senders()
            if not senders:
                raise RuntimeError("no node has enough confirmed balance for another payment")
            sender = self.randomizer.choice(senders)
            recipients = [node for node in self.nodes if node.name != sender.name]
            recipient = self.randomizer.choice(recipients)
            recipient_fields = wallet_fields[recipient.name]
            self.log(
                f"payment {payment_index + 1}/{self.args.payment_count}: "
                f"{sender.name} -> {recipient.name} amount={self.args.payment_amount}"
            )
            self.run_command(
                str(BIN),
                "pay",
                recipient_fields["public key"],
                str(self.args.payment_amount),
                "--home",
                str(sender.home),
            )
            self.wait_for_empty_mempools()
            self.wait_for_cluster_sync(self.nodes[0])
            if interval > 0:
                time.sleep(interval)

    def log_wallets(self) -> None:
        """Logs each node's public key for later manual inspection."""
        for node in self.nodes:
            fields = self.wallet_info(node)
            self.log(f"{node.name} public key: {fields['public key']}")

    def log_final_state(self) -> None:
        """Logs final height, balance, and log paths for all managed nodes."""
        for node in self.nodes:
            status = self.status_info(node)
            balance = self.balance_info(node)
            self.log(
                f"final {node.name} height={status['height']} balance={balance['total']} "
                f"best_tip={status['best tip']}"
            )
            self.log(f"{node.name} log: {node.log_path}")
        self.log(f"command log: {self.command_log}")

    def terminate_node(self, node: NodeHandle) -> None:
        """Stops one child process without failing cleanup on already-exited children."""
        if node.process is None:
            return
        if node.process.poll() is not None:
            return
        self.log(f"stopping {node.name} server")
        node.process.terminate()
        try:
            node.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.log(f"{node.name} server did not exit on SIGTERM; sending SIGKILL")
            node.process.kill()
            node.process.wait(timeout=5)

    def log_recent_server_output(self, lines: int = 40) -> None:
        """Prints the tail of each node stdout and structured server log."""
        for node in self.nodes:
            if not node.log_path.exists():
                stdout_tail = ""
            else:
                self.log(f"last {lines} lines from {node.name} stdout log:")
                text = node.log_path.read_text(encoding="utf-8", errors="replace")
                stdout_tail = "\n".join(text.splitlines()[-lines:])
                if stdout_tail:
                    self.append_output(stdout_tail)

            structured_logs = sorted((node.home / "logs").glob("server.log*"))
            if structured_logs:
                structured_log = structured_logs[-1]
                self.log(
                    f"last {lines} lines from {node.name} structured log ({structured_log.name}):"
                )
                text = structured_log.read_text(encoding="utf-8", errors="replace")
                tail = "\n".join(text.splitlines()[-lines:])
                if tail:
                    self.append_output(tail)

    def cleanup(self) -> None:
        """Stops all managed servers and closes their log files."""
        for node in self.nodes:
            self.terminate_node(node)
            if node.log_handle is not None:
                node.log_handle.close()

    def run(self) -> int:
        """Runs the complete local configurable testnet harness."""
        try:
            self.log(f"run dir: {self.run_dir}")
            self.build_binary()
            self.create_nodes()
            self.init_homes()
            self.configure_homes()
            self.start_servers()
            self.log_wallets()
            self.bootstrap()
            self.run_random_payments()
            self.log_final_state()
            self.log("local testnet completed successfully")
            return 0
        except Exception as err:
            self.log(f"local testnet failed: {err}")
            self.log_recent_server_output()
            self.log(f"artifacts left in {self.run_dir}")
            return 1
        finally:
            self.cleanup()


def main() -> int:
    """Validates CLI args and runs one `LocalTestNet` instance."""
    args = parse_args()
    if args.nodes < 2:
        raise SystemExit("--nodes must be at least 2")
    if not 0 <= args.mining_node < args.nodes:
        raise SystemExit("--mining-node must reference one created node")
    return LocalTestNet(args).run()


if __name__ == "__main__":
    sys.exit(main())
