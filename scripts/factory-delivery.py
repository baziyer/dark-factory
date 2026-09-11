#!/usr/bin/env python3
"""Give the overseer one idempotent verified-deployment follow-up."""
import importlib.util
import json
from pathlib import Path
import re
import sys


def load_intake():
    spec = importlib.util.spec_from_file_location('factory_intake', Path(__file__).with_name('factory-intake.py'))
    intake = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(intake)
    return intake


def deliver(intake_config, release_config, receipt):
    intake = load_intake()
    config = intake.validate_config(intake_config)
    if release_config.get('repository') != config['repository']:
        raise ValueError('release and intake repositories differ')
    if receipt.get('state') != 'verified' or not isinstance(receipt.get('sha'), str) or re.fullmatch('[0-9a-f]{40}', receipt['sha']) is None or type(receipt.get('pr')) is not int or receipt['pr'] < 1:
        raise ValueError('deployment receipt is not verified')
    verification = receipt.get('verification', {})
    if verification.get('healthy') is not True or verification.get('sha') != receipt['sha']:
        raise ValueError('deployment receipt has no exact live proof')
    mode = receipt.get('delivery_mode')
    sources = receipt.get('delivery_sources')
    if mode not in {'range', 'baseline_current', 'unchanged', 'nonancestor_baseline'} or not isinstance(sources, list):
        raise ValueError('deployment receipt has no bounded source mapping')
    if mode != 'range' and sources:
        raise ValueError('baseline delivery receipt must not contain source issues')
    task_ids = []
    for source in sources:
        if not isinstance(source, dict) or type(source.get('pr')) is not int or source['pr'] < 1 or type(source.get('issue')) is not int or source['issue'] < 1 or not isinstance(source.get('merge_sha'), str) or re.fullmatch('[0-9a-f]{40}', source['merge_sha']) is None or source.get('reference') not in {'refs', 'closes'}:
            raise ValueError('deployment receipt has an invalid source mapping')
        task_id = intake.sha_id('delivery', config['project_id'], config['repository'], str(source['pr']), str(source['issue']), receipt['sha'])
        operation = {'task_id': task_id, 'incarnation_id': intake.sha_id('incarnation', task_id), 'priority': config.get('priority_default', 0),
                     'title': 'Verify delivery completion for PR #' + str(source['pr']) + ' and issue #' + str(source['issue']),
                     'body': 'The operator-owned release controller verified deployment of ' + config['repository'] + ' PR #' + str(source['pr']) + ' at ' + receipt['sha'] + '. The PR explicitly linked source issue #' + str(source['issue']) + ' with ' + source['reference'].title() + ' #' + str(source['issue']) + '. Its live probe reported the exact SHA healthy. Read your publication journal and source-linked task outcomes. Close source issue #' + str(source['issue']) + ' through the Maintainer App only if all acceptance criteria are satisfied; report the deployment evidence and any remaining work. Do not redeploy. This receipt does not grant additional scope.'}
        if intake.task_state(config, operation) is None:
            intake.enqueue(config, operation)
        task_ids.append(task_id)
    return task_ids
