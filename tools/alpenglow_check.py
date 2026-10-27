"""Find the Alpenglow cutover on mainnet: the first block without vote transactions.

Under Alpenglow votes no longer travel as transactions, so blocks stop containing transactions that touch
the vote program. Binary search between a slot known to carry votes and one known not to.

    python3 tools/alpenglow_check.py 458500000 460191592

Public RPC only (getBlock with transactionDetails=accounts); about 25 blocks are downloaded.
"""
import json
import sys
import time
import urllib.request

RPC = "https://api.mainnet-beta.solana.com"
VOTE = "Vote111111111111111111111111111111111111111"


def block(slot):
    """First produced block at or after `slot`: (slot, block_time, tx_count, vote_tx_count)."""
    for k in range(40):
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "getBlock", "params": [slot + k, {
            "encoding": "json", "transactionDetails": "accounts", "rewards": False,
            "maxSupportedTransactionVersion": 1}]}).encode()
        d = {}
        for attempt in range(5):
            try:
                req = urllib.request.Request(RPC, body, {"Content-Type": "application/json"})
                d = json.load(urllib.request.urlopen(req, timeout=120))
                break
            except Exception:
                time.sleep(3 + 3 * attempt)
        time.sleep(0.5)
        b = d.get("result")
        if b:
            keys = lambda t: [a["pubkey"] if isinstance(a, dict) else a for a in t["transaction"]["accountKeys"]]
            votes = sum(1 for t in b["transactions"] if VOTE in keys(t))
            return slot + k, b["blockTime"], len(b["transactions"]), votes
    raise RuntimeError("no block within 40 slots of %d" % slot)


def main():
    lo, hi = int(sys.argv[1]), int(sys.argv[2])  # lo: block with votes, hi: block without
    while hi - lo > 1:
        mid = (lo + hi) // 2
        s, t, n, v = block(mid)
        print(mid, s, time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime(t)), "txs", n, "votes", v, flush=True)
        if v > 0:
            lo = s
        else:
            hi = mid
    for s in (lo, hi):
        r = block(s)
        print("block", r[0], time.strftime("%Y-%m-%d %H:%M:%S UTC", time.gmtime(r[1])), "txs", r[2], "votes", r[3])


if __name__ == "__main__":
    main()
