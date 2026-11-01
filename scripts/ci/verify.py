#!/usr/bin/env python3
"""Candidate-bound local and CI verification interface."""
import argparse
import json
from pathlib import Path
import contract


def main():
    parser = argparse.ArgumentParser(description=__doc__, epilog=(
        "Profiles expose their tools, native capabilities and exclusions in inventory. "
        "Inventory never compiles. Missing evidence exits 3. Existing ci.sh remains authoritative."))
    sub = parser.add_subparsers(dest="command", required=True)
    select = sub.add_parser("select")
    select.add_argument("--event", choices=contract.EVENTS, required=True)
    select.add_argument("--candidate", required=True)
    select.add_argument("--base")
    select.add_argument("--changed-paths", type=Path)
    select.add_argument("--output", type=Path, required=True)
    select.add_argument("--size-table", action="store_true", help="explicitly select the existing optional size table")
    aggregate = sub.add_parser("aggregate")
    aggregate.add_argument("--selection", type=Path, required=True)
    aggregate.add_argument("--results", type=Path, required=True)
    inventory = sub.add_parser("inventory", help="Reconcile source and supplied discovery; missing captures stay incomplete")
    inventory.add_argument("--root", type=Path, default=contract.ROOT)
    inventory.add_argument("--captures", type=Path)
    inventory.add_argument("--candidate")
    inventory.add_argument("--output", type=Path, required=True)
    run = sub.add_parser("run", help="profile interface; execution requires the admitted T1-T5 runners")
    run.add_argument("--profile", choices=("focused", "core", "qualification", "release"), required=True)
    run.add_argument("--package", action="append", default=[])
    run.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    policy, policy_hash = contract.load_policy()
    if args.command == "run":
        if args.profile == "focused" and not args.package:
            raise contract.InvalidEvidence("focused requires an explicit --package scope")
        groups = ["quality", "functional"] if args.profile in ("focused", "core") else list(policy["profiles"])
        args.output.mkdir(parents=True, exist_ok=True)
        value = {"version": 1, "status": "incomplete", "child_exit": 3,
                 "profile": args.profile, "packages": args.package,
                 "policy_sha256": policy_hash,
                 "prerequisites": {g: policy["profiles"][g] for g in groups},
                 "exclusions": [g for g in policy["profiles"] if g not in groups],
                 "reason": "T1 preflight and T2-T5 profile dispatch are not implemented; no child ran"}
        with (args.output / "result.json").open("x") as output:
            output.write(json.dumps(value, indent=2) + "\n")
        print(json.dumps(value, indent=2))
        return 3
    if args.command == "inventory":
        candidate = args.candidate
        if candidate is None:
            candidate = contract.subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=args.root, text=True).strip()
        value, status = contract.inventory(args.root, args.captures, candidate, policy, policy_hash)
        args.output.write_text(json.dumps(value, indent=2) + "\n")
        return status
    if args.command == "aggregate":
        value, status = contract.aggregate(contract.read_json(args.selection), args.results,
                                           policy, policy_hash)
        print(json.dumps(value, indent=2))
        return status
    value = contract.select(contract.ROOT, policy, policy_hash, args.event, args.candidate, args.base,
                            args.changed_paths, args.size_table)
    args.output.write_text(json.dumps(value, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (contract.InvalidEvidence, OSError, UnicodeError, KeyError, TypeError, ValueError) as error:
        print(json.dumps({"version": 1, "status": "incomplete", "discrepancies": [str(error)]}))
        raise SystemExit(3)
