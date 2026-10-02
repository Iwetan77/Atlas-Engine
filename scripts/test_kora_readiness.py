import copy
import importlib.util
import json
from pathlib import Path
import unittest

ROOT = Path(__file__).parent
spec = importlib.util.spec_from_file_location("kora_check", ROOT / "check-kora.py")
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


class KoraPolicyTests(unittest.TestCase):
    def setUp(self):
        self.capture = json.loads((ROOT / "fixtures/kora-policy.json").read_text())
        self.config = copy.deepcopy(self.capture["config"])
        self.quote = copy.deepcopy(self.capture["quote"])
        self.payer = self.config["fee_payers"][0]

    def report(self):
        return check.policy_report(self.config, self.quote, self.payer, [check.USDC])

    def test_live_provider_excludes_jupiter_and_wrapped_sol(self):
        report = self.report()
        self.assertFalse(report["jupiterPolicyAllowsQuote"])
        self.assertTrue(report["acceptsUsdcFees"])
        self.assertEqual(report["unsupportedPrograms"], ["JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"])
        self.assertEqual(report["unsupportedTokens"], [check.SOL])

    def test_all_quote_programs_and_tokens_must_be_allowed(self):
        self.config["validation_config"]["allowed_programs"] = check.swap_programs(self.quote)
        self.config["validation_config"]["allowed_tokens"].append(check.SOL)
        self.assertTrue(self.report()["jupiterPolicyAllowsQuote"])
        self.quote["otherInstructions"] = [{"programId": "unknown-program"}]
        self.assertFalse(self.report()["jupiterPolicyAllowsQuote"])

    def test_configuration_payer_and_quoted_pair_are_bound(self):
        self.payer = "other-payer"
        with self.assertRaises(ValueError): self.report()
        self.payer = self.config["fee_payers"][0]
        self.quote["inAmount"] = "500001"
        with self.assertRaises(ValueError): self.report()
        self.quote["inAmount"] = "500000"
        self.quote["inputMint"] = check.SOL
        with self.assertRaises(ValueError): self.report()

    def test_missing_policy_and_disabled_signing_fail_closed(self):
        self.config["validation_config"] = {}
        self.assertFalse(self.report()["jupiterPolicyAllowsQuote"])
        self.config = copy.deepcopy(self.capture["config"])
        self.config["enabled_methods"]["sign_transaction"] = False
        self.assertIn("sign_transaction", self.report()["disabledMethods"])

    def test_audit_cannot_sign_or_submit(self):
        for method in ("signTransaction", "signAndSendTransaction", "sendTransaction", "raw_sign"):
            with self.assertRaises(ValueError): check.rpc("https://example.com", method, [])

    def test_credentials_and_insecure_endpoints_are_refused(self):
        for url in ("http://example.com", "https://key@example.com", "https://example.com/#secret"):
            with self.assertRaises(ValueError): check.endpoint(url)


if __name__ == "__main__":
    unittest.main()
