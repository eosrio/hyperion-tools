#!/usr/bin/env python3
"""Optional real-log qualification. Live inputs are read only; use disposable outputs."""
import argparse

from qualification import bench, corpus, damage, describe, fixtures, interrupt, mutate


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    corpus.add_parser(commands)
    bench.add_parser(commands)
    fixtures.add_parser(commands)
    damage.add_parser(commands)
    describe.add_parser(commands)
    interrupt.add_parser(commands)
    mutate.add_parser(commands)
    args = parser.parse_args()
    args.run(args)


if __name__ == "__main__":
    main()
