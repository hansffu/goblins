"""Activity reports use endpoint identity and publish through real subscriptions."""
import select
import unittest
from daemon_support import Daemon, RPC, receive

class ActivityTests(unittest.TestCase):
    def test_authority_freshness_recovery_and_lifecycle(self):
        d = Daemon(); self.addCleanup(d.close)
        a = d.start()['session']; b = d.start()['session']
        peer = RPC(d.state/a/'resources/request.sock'); self.addCleanup(peer.close)
        with self.assertRaises(ValueError): d.rpc.call('sessions.set-state', {'state':'working'})
        for value in ({'state':'busy'}, {'state':'idle','session':b}, {'state':'idle','target':b}):
            p = RPC(d.state/a/'resources/request.sock')
            try:
                with self.assertRaises(ValueError): p.call('sessions.set-state',value)
            finally: p.close()
        sub = d.host(); self.addCleanup(sub.close)
        sub.call('state.subscribe',{})
        peer.call('sessions.set-state',{'state':'working'})
        def event_state(state):
            while True:
                snapshot = receive(sub.peer)['params']['snapshot']
                record = next(s for s in snapshot['sessions'] if s['id'] == a)
                if record['agent_state'] == state: return record
        self.assertEqual(event_state('working')['state'],'running')
        p = RPC(d.state/a/'resources/request.sock')
        p.call('sessions.set-state',{'state':'working'}); p.close()
        self.assertFalse(select.select([sub.peer],[],[],.2)[0], 'heartbeat published unchanged activity')
        sub.peer.settimeout(35)
        self.assertEqual(event_state('unknown')['state'],'running')
        p = RPC(d.state/a/'resources/request.sock')
        p.call('sessions.set-state',{'state':'waiting-approval'}); p.close()
        event_state('waiting-approval')
        self.assertEqual(d.get(b)['agent_state'],'unknown')
        d.rpc.call('sessions.stop',{'session':a})
        self.assertNotEqual(event_state('unknown')['state'],'running')
        self.assertEqual(d.get(b)['state'],'running')

if __name__ == '__main__': unittest.main()
