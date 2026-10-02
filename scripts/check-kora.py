#!/usr/bin/env python3
"""Read-only provider policy check. Never requests a signature or sends a transaction."""
import argparse
import json
import os
import sys
import urllib.parse
import urllib.request

USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
SOL = "So11111111111111111111111111111111111111112"
MAINNET = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d"


def endpoint(value):
    parsed = urllib.parse.urlsplit(value)
    if parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password or parsed.fragment:
        raise ValueError("Use an HTTPS endpoint without embedded credentials or a fragment")
    return value


def read(url, body=None, key=None):
    headers = {"User-Agent": "atlas-engine", "Content-Type": "application/json"}
    if key:
        headers["x-api-key"] = key
    request = urllib.request.Request(endpoint(url),
        data=None if body is None else json.dumps(body).encode(), headers=headers)
    with urllib.request.urlopen(request, timeout=20) as response:
        data = response.read(2_000_001)
    if len(data) > 2_000_000:
        raise ValueError("Provider response was too large")
    return json.loads(data)


def rpc(url, method, params, key=None):
    # There is deliberately no signing or submission method in this allowlist.
    if method not in {"getConfig", "getPayerSigner", "getSupportedTokens", "getBlockhash",
                       "getGenesisHash", "isBlockhashValid", "getBalance"}:
        raise ValueError("This check only permits read-only requests")
    body = read(url, {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}, key)
    if body.get("error") is not None or "result" not in body:
        raise ValueError("Provider could not answer " + method)
    return body["result"]


def swap_programs(quote):
    if not isinstance(quote.get("swapInstruction"), dict):
        raise ValueError("Jupiter did not return a swap instruction")
    instructions = []
    for key in ("computeBudgetInstructions", "setupInstructions", "otherInstructions"):
        instructions.extend(quote.get(key) or [])
    for key in ("swapInstruction", "cleanupInstruction", "tipInstruction"):
        if quote.get(key):
            instructions.append(quote[key])
    return sorted({ix["programId"] for ix in instructions})


def policy_report(config, quote, payer, supported_tokens):
    rules = config.get("validation_config") or {}
    methods = config.get("enabled_methods") or {}
    required = ("get_config", "get_payer_signer", "estimate_transaction_fee", "sign_transaction")
    disabled = [name for name in required if methods.get(name) is not True]
    if payer not in config.get("fee_payers", []):
        raise ValueError("Provider payer does not match its configuration")
    if quote.get("inputMint") != USDC or quote.get("outputMint") != SOL or quote.get("inAmount") != "500000":
        raise ValueError("Jupiter changed the requested pair or amount")
    if int(quote.get("outAmount", "0")) <= 0:
        raise ValueError("Jupiter returned no output")
    unsupported = sorted(set(swap_programs(quote)) - set(rules.get("allowed_programs", [])))
    # wSOL is an SPL token even when the swap later unwraps it into native SOL.
    missing_tokens = sorted({USDC, SOL} - set(rules.get("allowed_tokens", [])))
    usdc_paid = USDC in supported_tokens and USDC in rules.get("allowed_spl_paid_tokens", [])
    return {"jupiterPolicyAllowsQuote": not (unsupported or missing_tokens or disabled) and usdc_paid,
        "unsupportedPrograms": unsupported, "unsupportedTokens": missing_tokens,
        "disabledMethods": disabled, "acceptsUsdcFees": usdc_paid}


def audit(args):
    kora = endpoint(args.kora)
    node = endpoint(args.rpc)
    if rpc(node, "getGenesisHash", []) != MAINNET:
        raise ValueError("The Solana RPC is not mainnet")
    config = rpc(kora, "getConfig", [], os.getenv("KORA_AUDIT_API_KEY"))
    payer = rpc(kora, "getPayerSigner", [], os.getenv("KORA_AUDIT_API_KEY"))["signer_address"]
    tokens = rpc(kora, "getSupportedTokens", [], os.getenv("KORA_AUDIT_API_KEY"))["tokens"]
    blockhash = rpc(kora, "getBlockhash", [], os.getenv("KORA_AUDIT_API_KEY"))["blockhash"]
    if not rpc(node, "isBlockhashValid", [blockhash, {"commitment": "confirmed"}])["value"]:
        raise ValueError("Provider blockhash is not live on mainnet; try again")
    balance = rpc(node, "getBalance", [payer, {"commitment": "confirmed"}])["value"]
    params = {"inputMint": USDC, "outputMint": SOL, "amount": "500000", "taker": args.wallet,
        "payer": payer, "slippageBps": "50", "computeUnitPricePercentile": "medium"}
    quote = read("https://api.jup.ag/swap/v2/build?" + urllib.parse.urlencode(params),
                 key=os.getenv("JUPITER_API_KEY"))
    report = policy_report(config, quote, payer, tokens)
    report.update({"mainnet": True, "feePayer": payer, "payerLamports": balance,
        "inputUsdcUnits": quote["inAmount"], "quotedOutputLamports": quote["outAmount"],
        "signaturesRequested": 0, "transactionsSubmitted": 0,
        "executionVerified": False})
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wallet", required=True, help="Public Solana address; never a key")
    parser.add_argument("--kora", default="https://mainnet.kora-nodes.com")
    parser.add_argument("--rpc", default="https://api.mainnet-beta.solana.com")
    args = parser.parse_args()
    try:
        report = audit(args)
    except Exception as error:
        # Requests may contain API keys in URLs. Do not print provider exception bodies or URLs.
        print(json.dumps({"checkComplete": False, "errorType": type(error).__name__}), file=sys.stderr)
        return 2
    print(json.dumps(report, indent=2))
    return 0 if report["jupiterPolicyAllowsQuote"] and report["payerLamports"] >= 10_000 else 1


if __name__ == "__main__":
    sys.exit(main())
