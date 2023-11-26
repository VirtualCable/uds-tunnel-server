#!/usr/bin/env python3
# -*- coding: utf-8 -*-
#
# Copyright (c) 2023 Virtual Cable S.L.U.
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
import asyncio
import logging
import os
import signal
import socket
import ssl
import threading  # event for stop notification
import typing

try:
    import uvloop  # type: ignore

    asyncio.set_event_loop_policy(uvloop.EventLoopPolicy())
except ImportError:
    pass  # no uvloop support


from uds_tunnel import config, consts, log, processes, proxy, stats

if typing.TYPE_CHECKING:
    from multiprocessing.connection import Connection
    from multiprocessing.managers import Namespace


logger = logging.getLogger(__name__)

do_stop: threading.Event = threading.Event()


def stop_signal(signum: int, frame: typing.Any) -> None:
    do_stop.set()
    logger.debug('SIGNAL %s, frame: %s', signum, frame)
    

def setup_signal_handlers() -> None:    
    # Setup signal handlers
    try:
        signal.signal(signal.SIGINT, stop_signal)
        signal.signal(signal.SIGTERM, stop_signal)
    except Exception as e:
        # Signal not available on threads, and we use threads on tests,
        # so we will ignore this because on tests signals are not important
        logger.warning('Signal not available: %s', e)



async def tunnel_proc_async(pipe: 'Connection', cfg: config.ConfigurationType) -> None:
    loop = asyncio.get_running_loop()

    tasks: typing.List[asyncio.Task] = []

    def add_autoremovable_task(task: asyncio.Task) -> None:
        tasks.append(task)

        def remove_task(task: asyncio.Task) -> None:
            logger.debug('Removing task %s', task)
            tasks.remove(task)

        task.add_done_callback(remove_task)

    def get_socket() -> typing.Tuple[typing.Optional[socket.socket], typing.Optional[typing.Tuple[str, int]]]:
        try:
            while True:
                # Clear back event, for next data
                msg: typing.Optional[typing.Tuple[socket.socket, typing.Tuple[str, int]]] = pipe.recv()
                if msg:
                    return msg
        except EOFError:
            logger.debug('Parent process closed connection')
            pipe.close()
            return None, None
        except Exception:
            logger.exception('Receiving data from parent process')
            pipe.close()
            return None, None

    async def run_server() -> None:
        # Generate SSL context
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        args: typing.Dict[str, typing.Any] = {
            'certfile': cfg.ssl_certificate,
        }
        if cfg.ssl_certificate_key:
            args['keyfile'] = cfg.ssl_certificate_key
        if cfg.ssl_password:
            args['password'] = cfg.ssl_password

        context.load_cert_chain(**args)

        # Set min version from string (1.2 or 1.3) as ssl.TLSVersion.TLSv1_2 or ssl.TLSVersion.TLSv1_3
        if cfg.ssl_min_tls_version in ('1.2', '1.3'):
            try:
                context.minimum_version = getattr(
                    ssl.TLSVersion, f'TLSv1_{cfg.ssl_min_tls_version.split(".")[1]}'
                )
            except Exception as e:
                logger.exception('Setting min tls version failed: %s. Using defaults', e)
                context.minimum_version = ssl.TLSVersion.TLSv1_2
        # Any other value will be ignored

        if cfg.ssl_ciphers:
            try:
                context.set_ciphers(cfg.ssl_ciphers)
            except Exception as e:
                logger.exception('Setting ciphers failed: %s. Using defaults', e)

        if cfg.ssl_dhparam:
            try:
                context.load_dh_params(cfg.ssl_dhparam)
            except Exception as e:
                logger.exception('Loading dhparams failed: %s. Using defaults', e)

        try:
            while True:
                address: typing.Optional[typing.Tuple[str, int]] = ('', 0)
                try:
                    (sock, address) = await loop.run_in_executor(None, get_socket)
                    if not sock:
                        break  # No more sockets, exit
                    logger.debug('CONNECTION from %s (pid: %s)', address, os.getpid())
                    # Due to proxy contains an "event" to stop, we need to create a new one for each connection
                    add_autoremovable_task(
                        asyncio.create_task(proxy.Proxy(cfg)(sock, context), name=f'proxy-{address}')
                    )
                except asyncio.CancelledError:  # pylint: disable=try-except-raise
                    raise  # Stop, but avoid generic exception (next line)
                except Exception:
                    logger.error('NEGOTIATION ERROR from %s', address[0] if address else 'unknown')
        except asyncio.CancelledError:
            pass  # Stop

    # create task for server

    add_autoremovable_task(asyncio.create_task(run_server(), name='server'))

    try:
        while tasks and not do_stop.is_set():
            to_wait = tasks[:]  # Get a copy of the list
            # Wait for "to_wait" tasks to finish, stop every 2 seconds to check if we need to stop
            # done, _ =
            await asyncio.wait(to_wait, return_when=asyncio.FIRST_COMPLETED, timeout=2)
    except asyncio.CancelledError:
        logger.info('Task cancelled')
        do_stop.set()  # ensure we stop
    except Exception:
        logger.exception('Error in main loop')
        do_stop.set()

    logger.debug('Out of loop, stopping tasks: %s, running: %s', tasks, do_stop.is_set())

    # If any task is still running, cancel it
    for task in tasks:
        try:
            task.cancel()
        except asyncio.CancelledError:
            pass  # Ignore, we are stopping

    # for task in tasks:
    #    task.cancel()

    # Wait for all tasks to finish
    await asyncio.wait(tasks, return_when=asyncio.ALL_COMPLETED)

    logger.info('PROCESS %s stopped', os.getpid())


def process_connection(client: socket.socket, addr: typing.Tuple[str, str], conn: 'Connection') -> None:
    data: bytes = b''
    try:
        # First, ensure handshake (simple handshake) and command
        data = client.recv(len(consts.HANDSHAKE_V1))

        if data != consts.HANDSHAKE_V1:
            raise Exception(f'Invalid data from {addr[0]}: {data.hex()}')  # Invalid handshake
        conn.send((client, addr))
        del client  # Ensure socket is controlled on child process
    except Exception as e:
        logger.error('HANDSHAKE invalid from %s: %s', addr[0], e)
        # Close Source and continue
        client.close()
