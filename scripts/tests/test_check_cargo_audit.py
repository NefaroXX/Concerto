import contextlib
import importlib.util
import io
from pathlib import Path
import sys
import unittest


spec = importlib.util.spec_from_file_location(
    "check_cargo_audit", Path(__file__).parents[1] / "check-cargo-audit.py"
)
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


class AuditGateTests(unittest.TestCase):
    def run_child(self, stdout="", stderr="", code=0):
        source = (
            f"import sys; sys.stdout.write({stdout!r}); "
            f"sys.stderr.write({stderr!r}); sys.exit({code})"
        )
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            result = audit.run_audit([sys.executable, "-c", source])
        return result, out.getvalue(), err.getvalue()

    def test_complete_clean_audit_passes_and_preserves_output(self):
        result, out, err = self.run_child("report\n", "Fetching advisory database\n")
        self.assertEqual(result, 0)
        self.assertEqual(out, "report\n")
        self.assertEqual(err, "Fetching advisory database\n")

    def test_nonzero_exit_is_preserved_without_diagnostic_dependency(self):
        self.assertEqual(self.run_child(code=2)[0], 2)

    def test_registry_error_with_zero_exit_fails(self):
        error = "error: couldn't check if the package is yanked: registry: request timed out\n"
        result, _, err = self.run_child(stderr=error)
        self.assertEqual(result, 1)
        self.assertIn(error, err)
        self.assertIn("despite exit zero", err)

    def test_error_on_stdout_also_fails(self):
        self.assertEqual(self.run_child(stdout="error: incomplete registry scan\n")[0], 1)

    def test_warning_with_zero_exit_fails(self):
        self.assertEqual(self.run_child(stderr="warning: package is yanked\n")[0], 1)

    def test_colored_diagnostic_is_not_hidden(self):
        self.assertEqual(self.run_child(stderr="\x1b[31merror:\x1b[0m scan failed\n")[0], 1)

    def test_missing_auditor_fails_explicitly(self):
        with contextlib.redirect_stderr(io.StringIO()) as err:
            result = audit.run_audit(["concerto-no-such-auditor-fixture"])
        self.assertEqual(result, 2)
        self.assertIn("unable to run", err.getvalue())


if __name__ == "__main__":
    unittest.main()
