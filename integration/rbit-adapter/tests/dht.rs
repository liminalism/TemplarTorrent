//! DHT announce over loopback with rbit's own nodes: B announces a port for
//! an info hash to A; a third node C then finds that port through A.
use rbit::DhtServer;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::time::timeout;

async fn node() -> (Arc<DhtServer>, SocketAddr) {
    let server = Arc::new(DhtServer::bind(0).await.unwrap());
    let address: SocketAddr = format!("127.0.0.1:{}", server.port()).parse().unwrap();
    let run = Arc::clone(&server);
    tokio::spawn(async move {
        let _ = run.run().await;
    });
    (server, address)
}

#[tokio::test]
async fn announce_peer_makes_us_findable() {
    timeout(Duration::from_secs(30), async {
        let (a, a_address) = node().await;
        let (b, _) = node().await;
        let (c, _) = node().await;
        b.ping(a_address).await.unwrap();
        c.ping(a_address).await.unwrap();
        assert!(
            b.routing_table().node_count() >= 1,
            "B learned A from its ping"
        );
        let hash = [0x5a; 20];
        let (found, announced) = b.get_peers_and_announce(hash, 51413).await.unwrap();
        assert!(found.is_empty(), "nobody announced before B");
        assert!(
            announced >= 1,
            "A accepted the announce with the token it issued"
        );
        let peers = c.get_peers(hash).await.unwrap();
        assert!(
            peers.contains(&"127.0.0.1:51413".parse().unwrap()),
            "C finds B's announced port through A: {peers:?}"
        );
        drop(a);
    })
    .await
    .unwrap();
}
