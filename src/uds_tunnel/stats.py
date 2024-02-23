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
import asyncio
import ctypes
import dataclasses
import io
import logging
import multiprocessing
import multiprocessing.sharedctypes
import socket
import ssl
import time
import typing

from . import config, consts

INTERVAL = 2  # Interval in seconds between stats update

logger = logging.getLogger(__name__)


class StatsSingleCounter:
    adder: typing.Callable[[int], None]

    def __init__(self, parent: 'StatsManager', for_receiver: bool=True) -> None:
        if for_receiver:
            self.adder = parent.add_recv
        else:
            self.adder = parent.add_sent

    def add(self, value: int) -> 'StatsSingleCounter':
        self.adder(value)
        return self


@dataclasses.dataclass
class StatsCounters:
    sent: 'multiprocessing.sharedctypes.Synchronized[int]'
    recv: 'multiprocessing.sharedctypes.Synchronized[int]'


class LocalStatsCounters:
    sent: int
    recv: int

    def __init__(self) -> None:
        self.sent = 0
        self.recv = 0


class StatsManager:
    connections_counter: typing.ClassVar[
        'multiprocessing.sharedctypes.Synchronized[int]'
    ] = multiprocessing.sharedctypes.Value(
        ctypes.c_int64, 0
    )  # type: ignore
    connections_total: typing.ClassVar[
        'multiprocessing.sharedctypes.Synchronized[int]'
    ] = multiprocessing.sharedctypes.Value(
        ctypes.c_int64, 0
    )  # type: ignore

    accum: typing.ClassVar[StatsCounters] = StatsCounters(
        multiprocessing.sharedctypes.Value(ctypes.c_int64, 0),  # type: ignore
        multiprocessing.sharedctypes.Value(ctypes.c_int64, 0),  # type: ignore
    )
    # No locking nor sharing needed for local stats
    local: typing.ClassVar[LocalStatsCounters] = LocalStatsCounters()
    partial_local: typing.ClassVar[LocalStatsCounters] = LocalStatsCounters()

    last: float  # timestamp, from time.monotonic()
    start_time: float  # timestamp, from time.monotonic()
    end_time: float  # timestamp, from time.monotonic()

    def __init__(self) -> None:
        self.last = self.start_time = self.end_time = self.current_time

    @property
    def current_time(self) -> float:
        return time.monotonic()

    @property
    def elapsed_time(self) -> float:
        return self.current_time - self.start_time

    def update(self, force: bool = False) -> None:
        """
        In order to keep stats updated, we will update them only if a certain time has passed
        or if force is True
        This reduces dramatically the number of locks needed, and keeps global stats reasonably updated
        """
        now = self.current_time
        if force or now - self.last > INTERVAL:
            self.last = now
            # Sinchronize accumulators
            with self.accum.sent:
                self.accum.sent.value += self.partial_local.sent
            with self.accum.recv:
                self.accum.recv.value += self.partial_local.recv
            # Reset partials
            self.partial_local.sent = self.partial_local.recv = 0

    def add_recv(self, size: int) -> None:
        self.local.recv += size
        self.partial_local.recv += size
        self.update()

    def add_sent(self, size: int) -> None:
        self.partial_local.sent += size
        self.local.sent += size
        self.update()

    def decrement_connections(self) -> None:
        # Decrement current runing connections
        with self.connections_counter:
            self.connections_counter.value -= 1

    def increment_connections(self) -> None:
        # Increment current runing connections
        # Also, increment total connections
        with self.connections_counter:
            self.connections_counter.value += 1
        with self.connections_total:
            self.connections_total.value += 1

    @property
    def as_sent_counter(self) -> 'StatsSingleCounter':
        return StatsSingleCounter(self, False)

    @property
    def as_recv_counter(self) -> 'StatsSingleCounter':
        return StatsSingleCounter(self, True)

    def close(self) -> None:
        self.decrement_connections()
        self.end_time = time.monotonic()
        self.update(True)  # Ensure that last values are updated

    @staticmethod
    def get_stats() -> typing.Generator['str', None, None]:
        # Do not lock because any variable, we want just an aproximation to current values
        # That, anyway, could change in the middle of the process
        yield ';'.join(
            [
                str(StatsManager.connections_counter.value),
                str(StatsManager.connections_total.value),
                str(StatsManager.accum.sent.value),
                str(StatsManager.accum.recv.value),
            ]
        )


# Stats processor, invoked from command line
async def get_server_stats(detailed: bool = False) -> None:
    cfg = config.read()

    # Context for local connection (ignores cert hostname)
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE  # For ServerStats, does not checks certificate

    try:
        host = cfg.listen_address if cfg.listen_address != '0.0.0.0' else 'localhost'
        reader: asyncio.StreamReader
        writer: asyncio.StreamWriter

        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.connect((host, cfg.listen_port))
            # Send HANDSHAKE
            sock.sendall(consts.HANDSHAKE_V1)
            # Ugrade connection to TLS
            reader, writer = await asyncio.open_connection(sock=sock, ssl=context, server_hostname=host)

            tmpdata = io.BytesIO()
            cmd = consts.COMMAND_STAT if detailed else consts.COMMAND_INFO

            writer.write(cmd + cfg.secret.encode())
            await writer.drain()

            while True:
                chunk = await reader.read(consts.BUFFER_SIZE)
                if not chunk:
                    break
                tmpdata.write(chunk)

        # Now we can output chunk data
        print(tmpdata.getvalue().decode())
    except Exception as e:
        print(e)
        return
