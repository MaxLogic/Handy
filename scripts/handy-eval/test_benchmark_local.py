"""Offline command contracts for the owned local-server launcher."""

from pathlib import Path
import tempfile
import unittest

import benchmark_local


class ServerCommandTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="handy-command-test-")
        self.addCleanup(self.temp.cleanup)
        self.model_path = Path(self.temp.name) / "model.gguf"
        self.model_path.touch()
        self.model = {
            "endpoint": "http://127.0.0.1:8123/v1",
            "model": "owned-model",
            "server": {"model_path": str(self.model_path), "context": 8192},
        }

    def command(self, **server_options):
        self.model["server"].update(server_options)
        return benchmark_local.build_server_command("llama-server.exe", self.model)

    def test_default_command_is_unchanged(self):
        self.assertEqual(self.command(), [
            "llama-server.exe", "-m", str(self.model_path.resolve()),
            "--alias", "owned-model", "--host", "127.0.0.1", "--port", "8123",
            "--ctx-size", "8192", "--parallel", "1", "--n-gpu-layers", "99",
            "--flash-attn", "on", "--reasoning", "off", "--jinja", "--temp", "0",
            "--threads", "16", "--sleep-idle-seconds", "-1",
        ])

    def test_explicit_speculation_modes_and_draft_limits(self):
        for options, suffix in [
            ({"spec_type": "none"}, ["--spec-type", "none"]),
            ({"spec_type": "draft-mtp"}, ["--spec-type", "draft-mtp"]),
            ({"spec_type": "draft-mtp", "spec_draft_n_max": 1},
             ["--spec-type", "draft-mtp", "--spec-draft-n-max", "1"]),
            ({"spec_type": "draft-mtp", "spec_draft_n_max": 16},
             ["--spec-type", "draft-mtp", "--spec-draft-n-max", "16"]),
        ]:
            with self.subTest(options=options):
                self.model["server"] = {"model_path": str(self.model_path)}
                command = self.command(**options)
                self.assertEqual(command[-len(suffix):], suffix)
                self.assertEqual(command[command.index("--ctx-size") + 1], "4096")

    def test_invalid_speculation_settings_are_rejected(self):
        invalid = [{"spec_type": value} for value in
                   (None, "draft", "DRAFT-MTP", "", 1, [], {})]
        invalid += [{"spec_type": "draft-mtp", "spec_draft_n_max": value}
                    for value in (None, 0, 17, -1, True, False, 1.0, "4", [])]
        invalid += [{"spec_draft_n_max": 4},
                    {"spec_type": "none", "spec_draft_n_max": 4}]
        for options in invalid:
            with self.subTest(options=options):
                self.model["server"] = {"model_path": str(self.model_path)}
                with self.assertRaises(ValueError):
                    self.command(**options)

    def test_only_explicit_loopback_port_is_owned(self):
        for endpoint in ("http://localhost:8123/v1", "http://127.0.0.1/v1",
                         "http://192.0.2.1:8123/v1"):
            with self.subTest(endpoint=endpoint):
                self.model["endpoint"] = endpoint
                with self.assertRaises(ValueError):
                    self.command()


if __name__ == "__main__":
    unittest.main()
