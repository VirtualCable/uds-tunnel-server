#!/usr/bin/env python3
# -*- coding: utf-8 -*-
#
# Copyright (c) 2022 Virtual Cable S.L.U.
# All rights reserved.
#
# Redistribution and use in source and binary forms, with or without modification,
# are permitted provided that the following conditions are met:
#
#    * Redistributions of source code must retain the above copyright notice,
#      this list of conditions and the following disclaimer.
#    * Redistributions in binary form must reproduce the above copyright notice,
#      this list of conditions and the following disclaimer in the documentation
#      and/or other materials provided with the distribution.
#    * Neither the name of Virtual Cable S.L. nor the names of its contributors
#      may be used to endorse or promote products derived from this software
#      without specific prior written permission.
#
# THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
# AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
# IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
# DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
# FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
# DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
# SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
# CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
# OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
# OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
'''
Author: Adolfo Gómez, dkmaster at dkmon dot com
'''
import argparse
import asyncio
import logging
import os
import pwd
import socket
import sys
from concurrent.futures import ThreadPoolExecutor

try:
    import uvloop

    asyncio.set_event_loop_policy(uvloop.EventLoopPolicy())
except ImportError:
    pass  # no uvloop support

try:
    import setproctitle
except ImportError:
    setproctitle = None  # type: ignore


from uds_tunnel import config, consts, log, processes, stats, tunnel_proc

logger = logging.getLogger(__name__)


def tunnel_main(args: 'argparse.Namespace') -> None:
    cfg = config.read(args.config)

    # Try to bind to port as running user
    # Wait for socket incoming connections and spread them
    socket.setdefaulttimeout(3.0)  # So we can check for stop from time to time and not block forever
    sock = socket.socket(
        socket.AF_INET6 if args.ipv6 or cfg.ipv6 or ':' in cfg.listen_address else socket.AF_INET,
        socket.SOCK_STREAM,
    )
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, True)
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    # We will not reuse port, we only want a UDS tunnel server running on a port
    # but this may change on future...
    # try:
    #     sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, True)
    # except (AttributeError, OSError) as e:
    #     logger.warning('socket.REUSEPORT not available')
    try:
        sock.bind((cfg.listen_address, cfg.listen_port))
        sock.listen(consts.BACKLOG)

        # If running as root, and requested drop privileges after port bind
        if os.getuid() == 0 and cfg.user:
            logger.debug('Changing to user %s', cfg.user)
            pwu = pwd.getpwnam(cfg.user)
            # os.setgid(pwu.pw_gid)
            os.setuid(pwu.pw_uid)

        log.setup_log(cfg)

        logger.info('Starting tunnel server on %s:%s', cfg.listen_address, cfg.listen_port)
        if setproctitle:
            setproctitle.setproctitle(f'UDSTunnel {cfg.listen_address}:{cfg.listen_port}')

        # Create pid file
        if cfg.pidfile:
            with open(cfg.pidfile, mode='w', encoding='utf-8') as f:
                f.write(str(os.getpid()))

    except Exception as e:
        sys.stderr.write(f'Tunnel startup error: {e}\n')
        logger.error('MAIN: %s', e)
        return

    tunnel_proc.setup_signal_handlers()

    prcs = processes.Processes(tunnel_proc.tunnel_proc_async, cfg)

    with ThreadPoolExecutor(max_workers=16) as executor:
        try:
            while not tunnel_proc.do_stop.is_set():
                try:
                    client, addr = sock.accept()
                    # logger.info('CONNECTION from %s', addr)

                    # Check if we have reached the max number of connections
                    # First part is checked on a thread, if HANDSHAKE is valid
                    # we will send socket to process pool
                    # Note: We use a thread pool here because we want to
                    #       ensure no denial of service is possible, or at least
                    #       we try to limit it (if connection delays too long, we will close it on the thread)
                    executor.submit(tunnel_proc.process_connection, client, addr, prcs.best_child())
                except socket.timeout:
                    pass  # Continue and retry
                except Exception as e:
                    logger.error('LOOP: %s', e)
        except Exception as e:
            sys.stderr.write(f'Error: {e}\n')
            logger.error('MAIN: %s', e)

    if sock:
        sock.close()

    prcs.stop()

    try:
        if cfg.pidfile:
            os.unlink(cfg.pidfile)
    except Exception:
        logger.warning('Could not remove pidfile %s', cfg.pidfile)

    logger.info('FINISHED')


def main() -> None:
    parser = argparse.ArgumentParser()
    group = parser.add_mutually_exclusive_group()
    group.add_argument('-t', '--tunnel', help='Starts the tunnel server', action='store_true')
    # group.add_argument('-r', '--rdp', help='RDP Tunnel for traffic accounting')
    group.add_argument(
        '-s',
        '--stats',
        help='get current global stats from RUNNING tunnel',
        action='store_true',
    )
    group.add_argument(
        '-d',
        '--detailed-stats',
        help='get current detailed stats from RUNNING tunnel',
        action='store_true',
    )
    # Config file
    parser.add_argument(
        '-c',
        '--config',
        help=f'Config file to use (default: {consts.CONFIGFILE})',
        default=consts.CONFIGFILE,
    )
    # If force ipv6
    parser.add_argument(
        '-6',
        '--ipv6',
        help='Force IPv6 for tunnel server',
        action='store_true',
    )
    args = parser.parse_args()

    if args.tunnel:
        tunnel_main(args)
    elif args.detailed_stats:
        asyncio.run(stats.get_server_stats(True))
    elif args.stats:
        asyncio.run(stats.get_server_stats(False))
    else:
        parser.print_help()


if __name__ == "__main__":
    main()
