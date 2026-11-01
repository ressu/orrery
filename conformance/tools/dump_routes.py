"""Dump upstream Moonraker's endpoint table as JSON.

Takes a Moonraker data path and an output file. Run with the reference venv (see the justfile `routes` recipe). It loads every
core component the same way Moonraker's own tests do, then prints the
APIDefinition cache: JSON-RPC method name -> HTTP verb and path. The
conformance runner uses this to send each JSON-RPC case over HTTP too.
"""
import asyncio
import json
import sys

from moonraker.common import APIDefinition, TransportType
from moonraker.eventloop import EventLoop
from moonraker.loghelper import LogManager
from moonraker.server import Server


async def main(data_path: str, out_file: str) -> None:
    args = {
        "data_path": data_path,
        "is_default_data_path": False,
        "config_file": f"{data_path}/config/moonraker.conf",
        "backup_config": None,
        "startup_warnings": [],
        "verbose": False,
        "debug": False,
        "asyncio_debug": False,
        "is_backup_config": False,
        "is_python_package": True,
        "instance_uuid": "dump-routes",
        "unix_socket_path": f"{data_path}/comms/moonraker.sock",
        "structured_logging": False,
        "software_version": "dump-routes",
        "git_branch": "?",
        "log_file": "",
        "python_version": sys.version,
        "launch_args": "dump-routes",
        "msgspec_enabled": False,
        "uvloop_enabled": False,
    }
    log_manager = LogManager(args, [])
    server = Server(args, log_manager, EventLoop())
    server.load_components()
    routes = {}
    for api_def in APIDefinition.get_cache().values():
        http = TransportType.HTTP in api_def.transports
        for rtype, method in api_def.rpc_items():
            routes[method] = {
                "verb": rtype.name if rtype.name else "POST",
                "path": api_def.http_path if http else None,
            }
    with open(out_file, "w") as f:
        json.dump(dict(sorted(routes.items())), f, indent=1)
        f.write("\n")


asyncio.run(main(sys.argv[1], sys.argv[2]))
