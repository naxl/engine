use crate::cloud_provider::service::Action;
use crate::models::application::ApplicationService;
use crate::models::container::ContainerService;
use crate::models::database::DatabaseService;
use crate::models::router::RouterService;
use crate::utilities::to_short_id;
use uuid::Uuid;

pub struct Environment {
    namespace: String,
    pub id: String,
    pub long_id: Uuid,
    pub project_id: String,
    pub project_long_id: Uuid,
    pub owner_id: String,
    pub organization_id: String,
    pub organization_long_id: Uuid,
    pub action: Action,
    pub applications: Vec<Box<dyn ApplicationService>>,
    pub containers: Vec<Box<dyn ContainerService>>,
    pub routers: Vec<Box<dyn RouterService>>,
    pub databases: Vec<Box<dyn DatabaseService>>,
}

impl Environment {
    pub fn new(
        long_id: Uuid,
        project_long_id: Uuid,
        organization_long_id: Uuid,
        action: Action,
        applications: Vec<Box<dyn ApplicationService>>,
        containers: Vec<Box<dyn ContainerService>>,
        routers: Vec<Box<dyn RouterService>>,
        databases: Vec<Box<dyn DatabaseService>>,
    ) -> Self {
        let project_id = to_short_id(&project_long_id);
        let env_id = to_short_id(&long_id);
        Environment {
            namespace: format!("{}-{}", project_id, env_id),
            id: env_id,
            long_id,
            project_id,
            project_long_id,
            owner_id: "FAKE".to_string(),
            organization_id: to_short_id(&organization_long_id),
            organization_long_id,
            action,
            applications,
            containers,
            routers,
            databases,
        }
    }

    pub fn namespace(&self) -> &str {
        self.namespace.as_str()
    }
}
