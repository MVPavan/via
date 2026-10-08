"""Fake-only effective-config publication regressions; OpenCode packet §13."""
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import opencode_driver as transport
import opencode_reply_tests as reply_tests
import opencode_safety as safety

OLD='http://127.0.0.1:1101/v1'
NEW='http://127.0.0.1:1102/v1'
THIRD='http://127.0.0.1:1103/v1'


def config(endpoint,provider='oclive-mock'):
    return {'model':safety.MOCK_IDENTITY,
        'agents':{name:{'model':safety.MOCK_IDENTITY} for name in
                  ('via','title','summary','compaction','explore','general','build','plan')},
        'providers':{provider:{'settings':{'baseURL':endpoint}}}}


def reply(info):
    return 200,json.dumps({'data':[{'type':'document','info':info}]}).encode()


class ConfigTests(unittest.TestCase):
    def driver(self,root):
        d=reply_tests.ReplyTests().driver(root,{})
        d.project.mkdir();d.vendor_identity=safety.Identity(71,123);d.proc=mock.Mock()
        d.mock_origins={safety.loopback_origin(value) for value in (OLD,NEW,THIRD)}
        d.provider_endpoints={'oclive-mock':OLD}
        d._served_schema=mock.Mock(return_value={'components':{'schemas':{
            'Config.InfoEncoded':{'properties':{'model':{},'agents':{},'providers':{}}}}}})
        d._http.request.return_value=reply(config(OLD))
        d._auxiliary_bindings(safety.MOCK_IDENTITY)  # Verified served map, before replacement.
        d._write_fixture_config(d.project,config(NEW))
        d.provider_endpoints={'oclive-mock':NEW};d._http.request.reset_mock()
        return d

    def test_verified_previous_and_empty_maps_wait_until_expected_map(self):
        with tempfile.TemporaryDirectory(prefix='via-occonfig-') as root:
            d=self.driver(root)
            d._http.request.side_effect=[reply(config(OLD)),reply({}),reply(config(NEW))]
            d.signals=mock.Mock()
            with mock.patch.object(transport.time,'sleep') as sleep:
                self.assertEqual(d._auxiliary_bindings(safety.MOCK_IDENTITY),[safety.MOCK_IDENTITY]*9)
            self.assertEqual(sleep.call_count,2)
            self.assertTrue(all(call.args[0]==.2 for call in sleep.call_args_list))
            self.assertEqual(d._http.request.call_count,3)
            self.assertGreaterEqual(d.proc.verify.call_count,8)
            self.assertEqual(d.signals.guard.call_count,3)

    def test_unapproved_maps_block_without_polling(self):
        for endpoint,provider in ((THIRD,'oclive-mock'),(NEW,'FAKE-unknown-provider'),
                                  ('https://example.invalid/v1','oclive-mock')):
            with self.subTest(endpoint=endpoint,provider=provider),tempfile.TemporaryDirectory(prefix='via-occonfig-') as root:
                d=self.driver(root);d._http.request.return_value=reply(config(endpoint,provider))
                with mock.patch.object(transport.time,'sleep') as sleep, self.assertRaises(safety.Blocked):
                    d._auxiliary_bindings(safety.MOCK_IDENTITY)
                sleep.assert_not_called();self.assertEqual(d._http.request.call_count,1)
                self.assertTrue(list(d.evidence.glob('*reply-block.json')))

    def test_deadline_is_phase_clipped_and_retains_the_last_config_reply(self):
        with tempfile.TemporaryDirectory(prefix='via-occonfig-') as root:
            d=self.driver(root);d._http.request.return_value=reply(config(OLD))
            clock=[0.0];d.phase_deadline=.25
            with mock.patch.object(transport.time,'monotonic',side_effect=lambda:clock[0]), \
                 mock.patch.object(transport.time,'sleep',side_effect=lambda seconds:clock.__setitem__(0,clock[0]+seconds)), \
                 self.assertRaisesRegex(safety.Blocked,'owned effective configuration readiness deadline'):
                d._auxiliary_bindings(safety.MOCK_IDENTITY)
            self.assertEqual(clock[0],.25)
            saved=reply_tests.ReplyTests().retained(d)
            self.assertEqual(saved['replies'][-1]['route'],'/api/config')
            self.assertEqual(saved['replies'][-1]['projection']['data'][0]['info']['providers']['oclive-mock']
                             ['settings']['baseURL']['host_port'],'127.0.0.1:1101')
            self.assertTrue(all(call.kwargs['timeout']<=.25 for call in d._http.request.call_args_list))

    def test_generation_change_blocks_before_the_next_read(self):
        with tempfile.TemporaryDirectory(prefix='via-occonfig-') as root:
            d=self.driver(root);d._http.request.return_value=reply(config(OLD))
            with mock.patch.object(transport.time,'sleep',side_effect=lambda _seconds:setattr(
                    d,'vendor_identity',safety.Identity(71,124))), \
                 self.assertRaisesRegex(safety.Blocked,'owned observation generation changed'):
                d._auxiliary_bindings(safety.MOCK_IDENTITY)
            self.assertEqual(d._http.request.call_count,1)

    def test_signal_blocks_before_the_next_read(self):
        with tempfile.TemporaryDirectory(prefix='via-occonfig-') as root:
            d=self.driver(root);d._http.request.return_value=reply(config(OLD))
            d.signals=mock.Mock();d.signals.guard.side_effect=[None,safety.Blocked('FAKE deferred signal')]
            with mock.patch.object(transport.time,'sleep'),self.assertRaisesRegex(safety.Blocked,'FAKE deferred signal'):
                d._auxiliary_bindings(safety.MOCK_IDENTITY)
            self.assertEqual(d._http.request.call_count,1)

    def test_unverified_previous_port_is_not_pending(self):
        with tempfile.TemporaryDirectory(prefix='via-occonfig-') as root:
            d=reply_tests.ReplyTests().driver(root,{})
            d.project.mkdir();d.mock_origins={safety.loopback_origin(value) for value in (OLD,NEW)}
            d._served_schema=mock.Mock(return_value={'components':{'schemas':{
                'Config.InfoEncoded':{'properties':{'model':{},'agents':{},'providers':{}}}}}})
            d._write_fixture_config(d.project,config(NEW))
            d.provider_endpoints={'oclive-mock':NEW};d._http.request.return_value=reply(config(OLD))
            with mock.patch.object(transport.time,'sleep') as sleep,self.assertRaisesRegex(
                    safety.Blocked,'effective provider endpoints differ from owned fixture'):
                d._auxiliary_bindings(safety.MOCK_IDENTITY)
            sleep.assert_not_called()

    def test_paid_selector_in_a_previous_map_blocks_without_polling(self):
        with tempfile.TemporaryDirectory(prefix='via-occonfig-') as root:
            d=self.driver(root);body=config(OLD);body['agents']['title']['model']='FAKE-paid/FAKE-model'
            d._http.request.return_value=reply(body)
            with mock.patch.object(transport.time,'sleep') as sleep,self.assertRaisesRegex(
                    safety.Blocked,'auxiliary model drift or paid identity'):
                d._auxiliary_bindings(safety.MOCK_IDENTITY)
            sleep.assert_not_called();self.assertTrue(d.guard.stopped)

    def test_satisfied_reload_does_not_allow_a_later_old_map(self):
        with tempfile.TemporaryDirectory(prefix='via-occonfig-') as root:
            d=self.driver(root);d._http.request.return_value=reply(config(NEW))
            d._auxiliary_bindings(safety.MOCK_IDENTITY)
            d._http.request.return_value=reply(config(OLD))
            with mock.patch.object(transport.time,'sleep') as sleep,self.assertRaisesRegex(
                    safety.Blocked,'effective provider endpoints differ from owned fixture'):
                d._auxiliary_bindings(safety.MOCK_IDENTITY)
            sleep.assert_not_called()

    def test_repository_namespace_write_waits_for_vendor_publication(self):
        with tempfile.TemporaryDirectory(prefix='via-occonfig-') as root:
            d=reply_tests.ReplyTests().driver(root,{})
            d.namespace=d.ownership.register(d.evidence/'state/vendor/opencode/FAKE-namespace','vendor-private')
            d.project=d.namespace;d.namespace.mkdir(parents=True,exist_ok=True)
            d.vendor_identity=safety.Identity(71,123);d.proc=mock.Mock()
            d.mock_origins={safety.loopback_origin(value) for value in (OLD,NEW)}
            d.provider_endpoints={'oclive-mock':OLD}
            d._served_schema=mock.Mock(return_value={'components':{'schemas':{
                'Config.InfoEncoded':{'properties':{'model':{},'agents':{},'providers':{}}}}}})
            d._http.request.return_value=reply(config(OLD))
            d._auxiliary_bindings(safety.MOCK_IDENTITY)
            (d.namespace/'opencode.json').write_text(json.dumps(config(OLD)))
            d._refresh_inventory=mock.Mock()
            def materialize(name,project,description):
                control=name=='ancestor-control'
                provider=mock.Mock();provider.receipt.return_value={'local_marker':True,
                    'ancestor_agents':control,'ancestor_skills':control,'models':['fixture-free'],'received':True}
                d.mock_providers[name]=provider
                d.provider_endpoints={'oclive-mock':NEW}
                return config(NEW)
            def turn(project,_prompt):
                d.project=project
                if project==d.namespace:
                    d._http.request.side_effect=[reply(config(OLD)),reply(config(NEW))]
                    d._last_auxiliary_bindings=d._auxiliary_bindings(safety.MOCK_IDENTITY)
            d._materialize_fixture=mock.Mock(side_effect=materialize)
            d._mock_turn=mock.Mock(side_effect=turn)
            d._instruction_facts=mock.Mock(return_value=[{'source':'FAKE-native-instruction-state'}])
            with mock.patch.object(safety,'init_fixture_repo',side_effect=lambda path,*_a,**_k:Path(path).mkdir(parents=True,exist_ok=True)), \
                 mock.patch.object(transport.time,'sleep') as sleep:
                self.assertTrue(d._repository_sentinel()['namespace_boundary'])
            sleep.assert_called_once_with(.2)

    def test_integration_is_fresh_after_effective_configuration_readiness(self):
        import opencode_driver_tests as driver_tests
        with tempfile.TemporaryDirectory(prefix='via-occonfig-') as root:
            d=driver_tests.DriverTests().catalog_driver(root,{'data':[]});order=[]
            def catalog(_identity):
                order.append('catalogue');return {safety.MOCK_IDENTITY:{'free':True}}
            def bindings(_identity):
                order.append('configuration-ready');return [safety.MOCK_IDENTITY]*9
            def integration(*_args,**_kwargs):
                order.append('integration');return 200,b'{"data":[]}'
            d._catalog=mock.Mock(side_effect=catalog);d._auxiliary_bindings=mock.Mock(side_effect=bindings)
            d._http.request.side_effect=integration
            self.assertTrue(d.spending_check(args=['spawn','--model',safety.MOCK_IDENTITY])['structural_spending_controls'])
            self.assertEqual(order,['catalogue','configuration-ready','integration'])
