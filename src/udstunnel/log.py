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
import logging
import sys
from logging.handlers import RotatingFileHandler

from udstunnel import config, consts

logger = logging.getLogger(__name__)

def setup_log(cfg: config.ConfigurationType) -> None:
    # Update logging if needed
    if cfg.logfile:
        fileh = RotatingFileHandler(
            filename=cfg.logfile,
            mode='a',
            maxBytes=cfg.logsize,
            backupCount=cfg.lognumber,
        )
        formatter = logging.Formatter(consts.LOGFORMAT)
        fileh.setFormatter(formatter)
        log = logging.getLogger()
        log.setLevel(cfg.loglevel)
        # for hdlr in log.handlers[:]:
        #     log.removeHandler(hdlr)
        log.addHandler(fileh)
    else:
        # Setup basic logging
        log = logging.getLogger()
        log.setLevel(cfg.loglevel)
        handler = logging.StreamHandler(sys.stderr)
        handler.setLevel(cfg.loglevel)
        formatter = logging.Formatter('%(levelname)s - %(message)s')  # Basic log format, nice for syslog
        handler.setFormatter(formatter)
        log.addHandler(handler)

    # If debug, print config
    if cfg.loglevel.lower() == 'debug':
        logger.debug('Configuration: %s', cfg)

