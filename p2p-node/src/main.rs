// src/main.rs
use libp2p::{
    core::upgrade,
    floodsub::{Floodsub, FloodsubEvent, Topic},
    identity,
    mdns::{Mdns, MdnsEvent},
    swarm::{NetworkBehaviourEventProcess, Swarm, SwarmBuilder},
    NetworkBehaviour, PeerId, Transport,
};
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use warp::Filter;

// Define the supported commands for our P2P network
#[derive(Debug, Serialize, Deserialize)]
enum OpenSkyCommand {
    ResourceOffer {
        cpu_cores: u8,
        memory_mb: u32,
        storage_gb: u32,
        bandwidth_mbps: u32,
        node_id: String,
    },
    TaskRequest {
        task_id: String,
        docker_image: String,
        cpu_cores: u8,
        memory_mb: u32,
        command: Vec<String>,
    },
    TaskResult {
        task_id: String,
        success: bool,
        result_data: String,
    },
    StorageRequest {
        file_id: String,
        size_bytes: u64,
    },
    StorageOffer {
        file_id: String,
        node_id: String,
        available: bool,
    },
}

// Our network behavior combines Floodsub for messaging and mDNS for peer discovery
#[derive(NetworkBehaviour)]
#[behaviour(event_process = true)]
struct OpenSkyBehaviour {
    floodsub: Floodsub,
    mdns: Mdns,
    #[behaviour(ignore)]
    response_sender: mpsc::UnboundedSender<OpenSkyCommand>,
}

impl NetworkBehaviourEventProcess<FloodsubEvent> for OpenSkyBehaviour {
    fn inject_event(&mut self, event: FloodsubEvent) {
        if let FloodsubEvent::Message(message) = event {
            if let Ok(command) = serde_json::from_slice::<OpenSkyCommand>(&message.data) {
                info!("Received command: {:?}", command);
                let _ = self.response_sender.send(command);
            }
        }
    }
}

impl NetworkBehaviourEventProcess<MdnsEvent> for OpenSkyBehaviour {
    fn inject_event(&mut self, event: MdnsEvent) {
        match event {
            MdnsEvent::Discovered(peers) => {
                for (peer_id, _addr) in peers {
                    info!("Discovered peer: {}", peer_id);
                    self.floodsub.add_node_to_partial_view(peer_id);
                }
            }
            MdnsEvent::Expired(peers) => {
                for (peer_id, _addr) in peers {
                    info!("Peer expired: {}", peer_id);
                    self.floodsub.remove_node_from_partial_view(&peer_id);
                }
            }
        }
    }
}

// In-memory storage for this prototype
struct OpenSkyNode {
    node_id: String,
    available_cpu: u8,
    available_memory: u32,
    available_storage: u32,
    available_bandwidth: u32,
    peers: HashSet<String>,
    tasks: Vec<String>,
    stored_files: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();

    // Create a random PeerId
    let id_keys = identity::Keypair::generate_ed25519();
    let peer_id = PeerId::from(id_keys.public());
    info!("Local peer id: {}", peer_id);

    // Parse configuration from environment variables
    let max_cpu_percent = env::var("OPENSKY_MAX_CPU_PERCENT")
        .unwrap_or_else(|_| "50".into())
        .parse::<u8>()?;
    
    let max_storage_gb = env::var("OPENSKY_MAX_STORAGE_GB")
        .unwrap_or_else(|_| "10".into())
        .parse::<u32>()?;
    
    let max_bandwidth_mbps = env::var("OPENSKY_MAX_BANDWIDTH_MBPS")
        .unwrap_or_else(|_| "50".into())
        .parse::<u32>()?;

    // Create data directory if it doesn't exist
    let data_dir = Path::new("/data");
    if !data_dir.exists() {
        fs::create_dir_all(data_dir)?;
    }

    // Set up the transport and swarm
    let (response_sender, mut response_rcv) = mpsc::unbounded_channel();

    // Create a transport with the Noise protocol for encryption
    let transport = libp2p::development_transport(id_keys).await?;

    // Create a Floodsub topic
    let floodsub_topic = Topic::new("opensky-network");

    // Create a Swarm to manage peers and events
    let mut behaviour = OpenSkyBehaviour {
        floodsub: Floodsub::new(peer_id),
        mdns: Mdns::new(Default::default()).await?,
        response_sender,
    };

    behaviour.floodsub.subscribe(floodsub_topic.clone());

    let mut swarm = SwarmBuilder::new(transport, behaviour, peer_id)
        .executor(Box::new(|fut| {
            tokio::spawn(fut);
        }))
        .build();

    // Initialize node state
    let node = Arc::new(Mutex::new(OpenSkyNode {
        node_id: peer_id.to_string(),
        available_cpu: max_cpu_percent,
        available_memory: system_info::mem_info().total as u32 / 2, // Use half of system RAM
        available_storage: max_storage_gb,
        available_bandwidth: max_bandwidth_mbps,
        peers: HashSet::new(),
        tasks: Vec::new(),
        stored_files: Vec::new(),
    }));

    // Listen on all interfaces and a random port
    swarm.listen_on("/ip4/0.0.0.0/tcp/30333".parse()?)?;

    // Create a clone of node for the web API
    let node_for_api = node.clone();

    // Set up the web API
    let node_routes = warp::path("api")
        .and(warp::path("node"))
        .and(warp::get())
        .map(move || {
            let node = node_for_api.lock().unwrap();
            warp::reply::json(&serde_json::json!({
                "node_id": node.node_id,
                "resources": {
                    "cpu": node.available_cpu,
                    "memory_mb": node.available_memory,
                    "storage_gb": node.available_storage,
                    "bandwidth_mbps": node.available_bandwidth
                },
                "peers": node.peers.len(),
                "tasks": node.tasks.len(),
                "files": node.stored_files.len()
            }))
        });

    // Start the web server
    let server = warp::serve(node_routes).run(([0, 0, 0, 0], 8080));
    tokio::spawn(server);

    // Clone the floodsub topic for the command loop
    let floodsub_topic_clone = floodsub_topic.clone();
    
    // Create a clone of swarm for the command loop
    let mut swarm_clone = swarm.clone();
    
    // Process incoming commands
    tokio::spawn(async move {
        while let Some(command) = response_rcv.recv().await {
            match command {
                OpenSkyCommand::ResourceOffer { node_id, .. } => {
                    info!("Received resource offer from: {}", node_id);
                    // In a real implementation, we would store this in a resource registry
                }
                OpenSkyCommand::TaskRequest { task_id, docker_image, cpu_cores, memory_mb, command } => {
                    info!("Received task request: {}", task_id);
                    // For the prototype, we'll just simulate task execution
                    
                    // Check if we have enough resources
                    let can_execute = {
                        let mut node = node.lock().unwrap();
                        if node.available_cpu >= cpu_cores {
                            // We would actually reserve these resources
                            node.available_cpu -= cpu_cores;
                            node.tasks.push(task_id.clone());
                            true
                        } else {
                            false
                        }
                    };
                    
                    if can_execute {
                        // Simulate task execution (in reality, we would run a Docker container)
                        info!("Executing task: {} using image: {}", task_id, docker_image);
                        
                        // Simulate task completion
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        
                        // Send back result
                        let result = OpenSkyCommand::TaskResult {
                            task_id: task_id.clone(),
                            success: true,
                            result_data: "Task completed successfully".into(),
                        };
                        
                        let json = serde_json::to_string(&result).expect("Failed to serialize");
                        swarm_clone.behaviour_mut().floodsub.publish(floodsub_topic_clone.clone(), json.as_bytes());
                        
                        // Release resources
                        let mut node = node.lock().unwrap();
                        node.available_cpu += cpu_cores;
                        node.tasks.retain(|t| t != &task_id);
                    }
                }
                OpenSkyCommand::StorageRequest { file_id, size_bytes } => {
                    info!("Received storage request for file: {}", file_id);
                    
                    // Check if we have enough storage
                    let can_store = {
                        let mut node = node.lock().unwrap();
                        let size_gb = (size_bytes / (1024 * 1024 * 1024)) as u32 + 1;
                        if node.available_storage >= size_gb {
                            // Reserve storage
                            node.available_storage -= size_gb;
                            node.stored_files.push(file_id.clone());
                            true
                        } else {
                            false
                        }
                    };
                    
                    // Send storage offer
                    let offer = OpenSkyCommand::StorageOffer {
                        file_id,
                        node_id: peer_id.to_string(),
                        available: can_store,
                    };
                    
                    let json = serde_json::to_string(&offer).expect("Failed to serialize");
                    swarm_clone.behaviour_mut().floodsub.publish(floodsub_topic_clone.clone(), json.as_bytes());
                }
                _ => {} // Handle other commands
            }
        }
    });

    // Periodically announce our resources
    let floodsub_topic_resources = floodsub_topic.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            
            let resource_offer = {
                let node = node.lock().unwrap();
                OpenSkyCommand::ResourceOffer {
                    cpu_cores: node.available_cpu,
                    memory_mb: node.available_memory,
                    storage_gb: node.available_storage,
                    bandwidth_mbps: node.available_bandwidth,
                    node_id: node.node_id.clone(),
                }
            };
            
            let json = serde_json::to_string(&resource_offer).expect("Failed to serialize");
            if let Ok(mut swarm) = swarm_clone.lock() {
                swarm.behaviour_mut().floodsub.publish(floodsub_topic_resources.clone(), json.as_bytes());
            }
        }
    });

    // Read full lines from stdin
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();

    // Kick it off
    info!("OpenSky node started. Available at http://localhost:8080");
    info!("Type 'help' for available commands");

    loop {
        tokio::select! {
            line = stdin.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(e) => {
                        error!("Error reading from stdin: {:?}", e);
                        break;
                    }
                };

                match line.as_str() {
                    "help" => {
                        info!("Available commands:");
                        info!("  peers - List connected peers");
                        info!("  resources - Show available resources");
                        info!("  status - Show node status");
                        info!("  quit - Exit the application");
                    }
                    "peers" => {
                        // In a real implementation, we would get this from the network behavior
                        let node = node.lock().unwrap();
                        info!("Connected peers: {}", node.peers.len());
                        for peer in &node.peers {
                            info!("  {}", peer);
                        }
                    }
                    "resources" => {
                        let node = node.lock().unwrap();
                        info!("Available resources:");
                        info!("  CPU: {}%", node.available_cpu);
                        info!("  Memory: {} MB", node.available_memory);
                        info!("  Storage: {} GB", node.available_storage);
                        info!("  Bandwidth: {} Mbps", node.available_bandwidth);
                    }
                    "status" => {
                        let node = node.lock().unwrap();
                        info!("Node ID: {}", node.node_id);
                        info!("Connected peers: {}", node.peers.len());
                        info!("Active tasks: {}", node.tasks.len());
                        info!("Stored files: {}", node.stored_files.len());
                    }
                    "quit" => break,
                    _ => error!("Unknown command: {}", line),
                }
            }
            event = swarm.select_next_some() => {
                info!("Swarm event: {:?}", event);
            }
        }
    }

    Ok(())
}