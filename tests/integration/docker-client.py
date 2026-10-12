#!/usr/bin/env python3
"""Test-only database client bridge. Streams files through disposable containers.

Never use with production servers. Every connection is to fixtures created by docker.sh.
"""
import json
import os
import pathlib
import subprocess
import sys
import urllib.parse

program = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
stdin = None
stdout = None
extra_env = []
if program in ('pg_dump', 'pg_restore'):
    container = os.environ['DBFORK_TEST_CONTAINER_PG']
    index = args.index('--dbname')
    database = urllib.parse.urlsplit(args[index + 1]).path.lstrip('/')
    args[index + 1] = database
    args.extend(['--username', 'postgres'])
    if program == 'pg_dump':
        index = args.index('--file')
        stdout = open(args[index + 1], 'wb')
        del args[index:index + 2]
    else:
        stdin = open(args[-3], 'rb')
        del args[-3]
elif program in ('mysql', 'mysqldump', 'mariadb', 'mariadb-dump'):
    container = os.environ['DBFORK_TEST_CONTAINER_MYSQL']
    args = [arg for arg in args if not arg.startswith('--defaults-extra-file=')]
    args.insert(0, '--user=root')
    extra_env = ['-e', 'MYSQL_PWD=dbfork_test']
else:
    container = os.environ['DBFORK_TEST_CONTAINER_MONGO']
    if program == 'mongosh':
        for name in ('DBFORK_MONGO_URL', 'DBFORK_MONGO_SOURCE', 'DBFORK_TEST_MONGO_URL'):
            if name in os.environ:
                u = urllib.parse.urlsplit(os.environ[name])
                value = urllib.parse.urlunsplit((u.scheme, '127.0.0.1:27017', u.path, u.query, u.fragment))
                extra_env += ['-e', name + '=' + value]
        if '--file' in args:
            index = args.index('--file')
            script = pathlib.Path(args[index + 1]).read_text()
            args[index:index + 2] = ['--eval', script]
    else:
        index = args.index('--config')
        config = json.loads(pathlib.Path(args[index + 1]).read_text())
        del args[index:index + 2]
        database = urllib.parse.urlsplit(config['uri']).path.lstrip('/')
        index = next(i for i,arg in enumerate(args) if arg.startswith('--archive='))
        archive = args[index].split('=', 1)[1]
        args[index] = '--archive'
        if program == 'mongodump':
            stdout = open(archive, 'wb')
            args.extend(['--db', database])
        else:
            stdin = open(archive, 'rb')
try:
    result = subprocess.run(['docker', 'exec', '-i', *extra_env, container, program, *args], stdin=stdin, stdout=stdout)
    sys.exit(result.returncode)
finally:
    if stdin is not None:
        stdin.close()
    if stdout is not None:
        stdout.close()
