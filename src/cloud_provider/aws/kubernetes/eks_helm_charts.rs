use crate::cloud_provider::aws::kubernetes::{Options, VpcQoveryNetworkMode};
use crate::cloud_provider::helm::{
    get_chart_for_cert_manager_config, get_chart_for_cluster_agent, get_chart_for_shell_agent,
    get_engine_helm_action_from_location, ChartInfo, ChartPayload, ChartSetValue, ChartValuesGenerated,
    ClusterAgentContext, CommonChart, CoreDNSConfigChart, HelmAction, HelmChart, HelmChartNamespaces,
    ShellAgentContext,
};
use crate::cloud_provider::io::ClusterAdvancedSettings;
use crate::cloud_provider::qovery::{get_qovery_app_version, EngineLocation, QoveryAppName, QoveryEngine};
use crate::cmd::helm_utils::CRDSUpdate;
use crate::cmd::kubectl::{kubectl_delete_crash_looping_pods, kubectl_exec_get_daemonset, kubectl_exec_with_output};
use crate::dns_provider::DnsProviderConfiguration;
use crate::errors::{CommandError, ErrorMessageVerbosity};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwsEksQoveryTerraformConfig {
    pub aws_iam_eks_user_mapper_key: String,
    pub aws_iam_eks_user_mapper_secret: String,
    pub aws_iam_cluster_autoscaler_key: String,
    pub aws_iam_cluster_autoscaler_secret: String,
    pub aws_iam_cloudwatch_key: String,
    pub aws_iam_cloudwatch_secret: String,
    pub loki_storage_config_aws_s3: String,
    pub aws_iam_loki_storage_key: String,
    pub aws_iam_loki_storage_secret: String,
}

pub struct EksChartsConfigPrerequisites {
    pub organization_id: String,
    pub organization_long_id: uuid::Uuid,
    pub cluster_id: String,
    pub cluster_long_id: uuid::Uuid,
    pub region: String,
    pub cluster_name: String,
    pub cloud_provider: String,
    pub test_cluster: bool,
    pub aws_access_key_id: String,
    pub aws_secret_access_key: String,
    pub vpc_qovery_network_mode: VpcQoveryNetworkMode,
    pub qovery_engine_location: EngineLocation,
    pub ff_log_history_enabled: bool,
    pub ff_metrics_history_enabled: bool,
    pub managed_dns_name: String,
    pub managed_dns_helm_format: String,
    pub managed_dns_resolvers_terraform_format: String,
    pub external_dns_provider: String,
    pub dns_email_report: String,
    pub acme_url: String,
    pub dns_provider_config: DnsProviderConfiguration,
    pub disable_pleco: bool,
    // qovery options form json input
    pub infra_options: Options,
    pub cluster_advanced_settings: ClusterAdvancedSettings,
}

pub fn eks_aws_helm_charts(
    qovery_terraform_config_file: &str,
    chart_config_prerequisites: &EksChartsConfigPrerequisites,
    chart_prefix_path: Option<&str>,
    kubernetes_config: &Path,
    envs: &[(String, String)],
) -> Result<Vec<Vec<Box<dyn HelmChart>>>, CommandError> {
    let content_file = match File::open(&qovery_terraform_config_file) {
        Ok(x) => x,
        Err(e) => {
            return Err(CommandError::new(
                "Can't deploy helm chart as Qovery terraform config file has not been rendered by Terraform. Are you running it in dry run mode?".to_string(),
                Some(e.to_string()),
                Some(envs.to_vec()),
            ));
        }
    };
    let chart_prefix = chart_prefix_path.unwrap_or("./");
    let chart_path = |x: &str| -> String { format!("{}/{}", &chart_prefix, x) };
    let reader = BufReader::new(content_file);
    let qovery_terraform_config: AwsEksQoveryTerraformConfig = match serde_json::from_reader(reader) {
        Ok(config) => config,
        Err(e) => {
            return Err(CommandError::new(
                format!("Error while parsing terraform config file {}", qovery_terraform_config_file),
                Some(e.to_string()),
                Some(envs.to_vec()),
            ));
        }
    };

    let prometheus_namespace = HelmChartNamespaces::Prometheus;
    let prometheus_internal_url = format!("http://prometheus-operated.{}.svc", prometheus_namespace);
    let loki_namespace = HelmChartNamespaces::Logging;
    let loki_kube_dns_name = format!("loki.{}.svc:3100", loki_namespace);

    // Qovery storage class
    let q_storage_class = CommonChart {
        chart_info: ChartInfo {
            name: "q-storageclass".to_string(),
            path: chart_path("/charts/q-storageclass"),
            ..Default::default()
        },
    };

    let mut aws_vpc_cni_chart = AwsVpcCniChart {
        chart_info: ChartInfo {
            name: "aws-vpc-cni".to_string(),
            path: chart_path("charts/aws-vpc-cni"),
            values: vec![
                ChartSetValue {
                    key: "image.region".to_string(),
                    value: chart_config_prerequisites.region.clone(),
                },
                ChartSetValue {
                    key: "init.image.region".to_string(),
                    value: chart_config_prerequisites.region.clone(),
                },
                ChartSetValue {
                    key: "image.pullPolicy".to_string(),
                    value: "IfNotPresent".to_string(),
                },
                ChartSetValue {
                    key: "crd.create".to_string(),
                    value: "false".to_string(),
                },
                // label ENIs
                ChartSetValue {
                    key: "env.CLUSTER_NAME".to_string(),
                    value: chart_config_prerequisites.cluster_name.clone(),
                },
                // number of total IP addresses that the daemon should attempt to allocate for pod assignment on the node (init phase)
                ChartSetValue {
                    key: "env.MINIMUM_IP_TARGET".to_string(),
                    value: "60".to_string(),
                },
                // number of free IP addresses that the daemon should attempt to keep available for pod assignment on the node
                ChartSetValue {
                    key: "env.WARM_IP_TARGET".to_string(),
                    value: "10".to_string(),
                },
                // maximum number of ENIs that will be attached to the node (k8s recommend to avoid going over 100)
                ChartSetValue {
                    key: "env.MAX_ENI".to_string(),
                    value: "100".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.cpu".to_string(),
                    value: "50m".to_string(),
                },
            ],
            ..Default::default()
        },
    };
    let is_cni_old_installed_version = match aws_vpc_cni_chart.is_cni_old_installed_version(kubernetes_config, envs) {
        Ok(x) => x,
        Err(e) => return Err(e),
    };
    aws_vpc_cni_chart.chart_info.values.push(ChartSetValue {
        key: "originalMatchLabels".to_string(),
        value: is_cni_old_installed_version.to_string(),
    });

    let aws_iam_eks_user_mapper = CommonChart {
        chart_info: ChartInfo {
            name: "iam-eks-user-mapper".to_string(),
            path: chart_path("charts/iam-eks-user-mapper"),
            values: vec![
                ChartSetValue {
                    key: "aws.accessKey".to_string(),
                    value: qovery_terraform_config.aws_iam_eks_user_mapper_key,
                },
                ChartSetValue {
                    key: "aws.secretKey".to_string(),
                    value: qovery_terraform_config.aws_iam_eks_user_mapper_secret,
                },
                ChartSetValue {
                    key: "aws.region".to_string(),
                    value: chart_config_prerequisites.region.clone(),
                },
                ChartSetValue {
                    key: "syncIamGroup".to_string(),
                    value: chart_config_prerequisites
                        .cluster_advanced_settings
                        .aws_iam_user_mapper_group_name
                        .clone(),
                },
                // resources limits
                ChartSetValue {
                    key: "resources.limits.cpu".to_string(),
                    value: "20m".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.cpu".to_string(),
                    value: "10m".to_string(),
                },
                ChartSetValue {
                    key: "resources.limits.memory".to_string(),
                    value: "32Mi".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.memory".to_string(),
                    value: "32Mi".to_string(),
                },
            ],
            ..Default::default()
        },
    };

    let aws_node_term_handler = CommonChart {
        chart_info: ChartInfo {
            name: "aws-node-term-handler".to_string(),
            path: chart_path("charts/aws-node-termination-handler"),
            values: vec![
                ChartSetValue {
                    key: "nameOverride".to_string(),
                    value: "aws-node-term-handler".to_string(),
                },
                ChartSetValue {
                    key: "fullnameOverride".to_string(),
                    value: "aws-node-term-handler".to_string(),
                },
                ChartSetValue {
                    key: "enableSpotInterruptionDraining".to_string(),
                    value: "true".to_string(),
                },
                ChartSetValue {
                    key: "enableScheduledEventDraining".to_string(),
                    value: "true".to_string(),
                },
                ChartSetValue {
                    key: "deleteLocalData".to_string(),
                    value: "true".to_string(),
                },
                ChartSetValue {
                    key: "ignoreDaemonSets".to_string(),
                    value: "true".to_string(),
                },
                ChartSetValue {
                    key: "podTerminationGracePeriod".to_string(),
                    value: "300".to_string(),
                },
                ChartSetValue {
                    key: "nodeTerminationGracePeriod".to_string(),
                    value: "120".to_string(),
                },
            ],
            ..Default::default()
        },
    };

    let aws_ui_view = CommonChart {
        chart_info: ChartInfo {
            name: "aws-ui-view".to_string(),
            path: chart_path("charts/aws-ui-view"),
            namespace: HelmChartNamespaces::KubeSystem,
            ..Default::default()
        },
    };

    let cluster_autoscaler = CommonChart {
        chart_info: ChartInfo {
            name: "cluster-autoscaler".to_string(),
            path: chart_path("common/charts/cluster-autoscaler"),
            values: vec![
                ChartSetValue {
                    key: "cloudProvider".to_string(),
                    value: chart_config_prerequisites.cloud_provider.clone(),
                },
                ChartSetValue {
                    key: "awsRegion".to_string(),
                    value: chart_config_prerequisites.region.clone(),
                },
                ChartSetValue {
                    key: "autoDiscovery.clusterName".to_string(),
                    value: chart_config_prerequisites.cluster_name.clone(),
                },
                ChartSetValue {
                    key: "awsAccessKeyID".to_string(),
                    value: qovery_terraform_config.aws_iam_cluster_autoscaler_key,
                },
                ChartSetValue {
                    key: "awsSecretAccessKey".to_string(),
                    value: qovery_terraform_config.aws_iam_cluster_autoscaler_secret,
                },
                // It's mandatory to get this class to ensure paused infra will behave properly on restore
                ChartSetValue {
                    key: "priorityClassName".to_string(),
                    value: "system-cluster-critical".to_string(),
                },
                // cluster autoscaler options
                ChartSetValue {
                    key: "extraArgs.balance-similar-node-groups".to_string(),
                    value: "true".to_string(),
                },
                // observability
                ChartSetValue {
                    key: "serviceMonitor.enabled".to_string(),
                    value: chart_config_prerequisites.ff_metrics_history_enabled.to_string(),
                },
                ChartSetValue {
                    key: "serviceMonitor.namespace".to_string(),
                    value: prometheus_namespace.to_string(),
                },
                // resources limits
                ChartSetValue {
                    key: "resources.limits.cpu".to_string(),
                    value: "100m".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.cpu".to_string(),
                    value: "100m".to_string(),
                },
                ChartSetValue {
                    key: "resources.limits.memory".to_string(),
                    value: "640Mi".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.memory".to_string(),
                    value: "640Mi".to_string(),
                },
            ],
            ..Default::default()
        },
    };

    let coredns_config = CoreDNSConfigChart {
        chart_info: ChartInfo {
            name: "coredns".to_string(),
            path: chart_path("/charts/coredns-config"),
            values: vec![
                ChartSetValue {
                    key: "managed_dns".to_string(),
                    value: chart_config_prerequisites.managed_dns_helm_format.clone(),
                },
                ChartSetValue {
                    key: "managed_dns_resolvers".to_string(),
                    value: chart_config_prerequisites
                        .managed_dns_resolvers_terraform_format
                        .clone(),
                },
            ],
            ..Default::default()
        },
    };

    let external_dns = CommonChart {
        chart_info: ChartInfo {
            name: "externaldns".to_string(),
            path: chart_path("common/charts/external-dns"),
            values_files: vec![chart_path("chart_values/external-dns.yaml")],
            values: vec![
                // resources limits
                ChartSetValue {
                    key: "resources.limits.cpu".to_string(),
                    value: "50m".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.cpu".to_string(),
                    value: "50m".to_string(),
                },
                ChartSetValue {
                    key: "resources.limits.memory".to_string(),
                    value: "50Mi".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.memory".to_string(),
                    value: "50Mi".to_string(),
                },
            ],
            ..Default::default()
        },
    };

    let promtail = CommonChart {
        chart_info: ChartInfo {
            name: "promtail".to_string(),
            last_breaking_version_requiring_restart: Some(Version::new(5, 1, 0)),
            path: chart_path("common/charts/promtail"),
            values_files: vec![chart_path("chart_values/promtail.yaml")],
            // because of priorityClassName, we need to add it to kube-system
            namespace: HelmChartNamespaces::KubeSystem,
            values: vec![
                ChartSetValue {
                    key: "config.clients[0].url".to_string(),
                    value: format!("http://{}/loki/api/v1/push", &loki_kube_dns_name),
                },
                // it's mandatory to get this class to ensure paused infra will behave properly on restore
                ChartSetValue {
                    key: "priorityClassName".to_string(),
                    value: "system-node-critical".to_string(),
                },
                // resources limits
                ChartSetValue {
                    key: "resources.limits.cpu".to_string(),
                    value: "100m".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.cpu".to_string(),
                    value: "100m".to_string(),
                },
                ChartSetValue {
                    key: "resources.limits.memory".to_string(),
                    value: "128Mi".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.memory".to_string(),
                    value: "128Mi".to_string(),
                },
            ],
            ..Default::default()
        },
    };

    let loki = CommonChart {
        chart_info: ChartInfo {
            name: "loki".to_string(),
            path: chart_path("common/charts/loki"),
            namespace: loki_namespace,
            timeout_in_seconds: 900,
            values_files: vec![chart_path("chart_values/loki.yaml")],
            values: vec![
                ChartSetValue {
                    key: "config.chunk_store_config.max_look_back_period".to_string(),
                    value: format!(
                        "{}w",
                        chart_config_prerequisites
                            .cluster_advanced_settings
                            .loki_log_retention_in_week
                    ),
                },
                ChartSetValue {
                    key: "config.table_manager.retention_period".to_string(),
                    value: format!(
                        "{}w",
                        chart_config_prerequisites
                            .cluster_advanced_settings
                            .loki_log_retention_in_week
                    ),
                },
                ChartSetValue {
                    key: "config.storage_config.aws.s3".to_string(),
                    value: qovery_terraform_config.loki_storage_config_aws_s3,
                },
                ChartSetValue {
                    key: "config.storage_config.aws.region".to_string(),
                    value: chart_config_prerequisites.region.clone(),
                },
                ChartSetValue {
                    key: "aws_iam_loki_storage_key".to_string(),
                    value: qovery_terraform_config.aws_iam_loki_storage_key,
                },
                ChartSetValue {
                    key: "aws_iam_loki_storage_secret".to_string(),
                    value: qovery_terraform_config.aws_iam_loki_storage_secret,
                },
                ChartSetValue {
                    key: "config.storage_config.aws.sse_encryption".to_string(),
                    value: "true".to_string(),
                },
                // resources limits
                ChartSetValue {
                    key: "resources.limits.cpu".to_string(),
                    value: "1".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.cpu".to_string(),
                    value: "300m".to_string(),
                },
                ChartSetValue {
                    key: "resources.limits.memory".to_string(),
                    value: "2Gi".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.memory".to_string(),
                    value: "1Gi".to_string(),
                },
            ],
            ..Default::default()
        },
    };

    /* Example to delete an old install
    let old_prometheus_operator = PrometheusOperatorConfigChart {
        chart_info: ChartInfo {
            name: "prometheus-operator".to_string(),
            namespace: prometheus_namespace,
            action: HelmAction::Destroy,
            ..Default::default()
        },
    };*/

    let kube_prometheus_stack = CommonChart {
        chart_info: ChartInfo {
            name: "kube-prometheus-stack".to_string(),
            path: chart_path("/common/charts/kube-prometheus-stack"),
            namespace: prometheus_namespace,
            // high timeout because on bootstrap, it's one of the biggest dependencies and on upgrade, it can takes time
            // to upgrade because of the CRD and the number of elements it has to deploy
            timeout_in_seconds: 480,
            crds_update: Some(CRDSUpdate{
                path:"https://raw.githubusercontent.com/prometheus-operator/prometheus-operator/v0.56.0/example/prometheus-operator-crd".to_string(), 
                resources: vec![
                    "monitoring.coreos.com_alertmanagerconfigs.yaml".to_string(),
                    "monitoring.coreos.com_alertmanagers.yaml".to_string(),
                    "monitoring.coreos.com_podmonitors.yaml".to_string(),
                    "monitoring.coreos.com_probes.yaml".to_string(),
                    "monitoring.coreos.com_prometheuses.yaml".to_string(),
                    "monitoring.coreos.com_prometheusrules.yaml".to_string(),
                    "monitoring.coreos.com_servicemonitors.yaml".to_string(),
                    "monitoring.coreos.com_thanosrulers.yaml".to_string(),
                ]
            }),
            values_files: vec![chart_path("chart_values/kube-prometheus-stack.yaml")],
            values: vec![
                ChartSetValue {
                    key: "installCRDs".to_string(),
                    value: "true".to_string(),
                },
                ChartSetValue {
                    key: "nameOverride".to_string(),
                    value: "prometheus-operator".to_string(),
                },
                ChartSetValue {
                    key: "fullnameOverride".to_string(),
                    value: "prometheus-operator".to_string(),
                },
                ChartSetValue {
                    key: "prometheus.prometheusSpec.externalUrl".to_string(),
                    value: prometheus_internal_url.clone(),
                },
                ChartSetValue {
                    key: "prometheusOperator.tls.enabled".to_string(),
                    value: "false".to_string(),
                },
                ChartSetValue {
                    key: "prometheusOperator.admissionWebhooks.enabled".to_string(),
                    value: "false".to_string(),
                },
                ChartSetValue {
                    key: "prometheus-node-exporter.prometheus.monitor.enabled".to_string(),
                    value: "false".to_string(),
                },
                ChartSetValue {
                    key: "grafana.serviceMonitor.enabled".to_string(),
                    value: "false".to_string(),
                },
                ChartSetValue {
                    key: "kubelet.serviceMonitor.resource".to_string(),
                    value: "true".to_string(),
                },
                ChartSetValue {
                    key: "kubelet.serviceMonitor.resourcePath".to_string(),
                    value: "/metrics/resource".to_string(),
                },


                // Limits prometheus-node-exporter
                ChartSetValue {
                    key: "prometheus-node-exporter.resources.limits.cpu".to_string(),
                    value: "20m".to_string(),
                },
                ChartSetValue {
                    key: "prometheus-node-exporter.resources.requests.cpu".to_string(),
                    value: "10m".to_string(),
                },
                ChartSetValue {
                    key: "prometheus-node-exporter.resources.limits.memory".to_string(),
                    value: "32Mi".to_string(),
                },
                ChartSetValue {
                    key: "prometheus-node-exporter.resources.requests.memory".to_string(),
                    value: "32Mi".to_string(),
                },
                // resources limits
                ChartSetValue {
                    key: "prometheusOperator.resources.limits.cpu".to_string(),
                    value: "1".to_string(),
                },
                ChartSetValue {
                    key: "prometheusOperator.resources.requests.cpu".to_string(),
                    value: "500m".to_string(),
                },
                ChartSetValue {
                    key: "prometheusOperator.resources.limits.memory".to_string(),
                    value: "1Gi".to_string(),
                },
                ChartSetValue {
                    key: "prometheusOperator.resources.requests.memory".to_string(),
                    value: "1Gi".to_string(),
                },
            ],
            ..Default::default()
        },
    };

    let prometheus_adapter = CommonChart {
        chart_info: ChartInfo {
            name: "prometheus-adapter".to_string(),
            path: chart_path("common/charts/prometheus-adapter"),
            last_breaking_version_requiring_restart: Some(Version::new(3, 3, 1)),
            namespace: prometheus_namespace,
            values: vec![
                ChartSetValue {
                    key: "metricsRelistInterval".to_string(),
                    value: "30s".to_string(),
                },
                ChartSetValue {
                    key: "prometheus.url".to_string(),
                    value: prometheus_internal_url.clone(),
                },
                ChartSetValue {
                    key: "podDisruptionBudget.enabled".to_string(),
                    value: "true".to_string(),
                },
                ChartSetValue {
                    key: "podDisruptionBudget.maxUnavailable".to_string(),
                    value: "1".to_string(),
                },
                // resources limits
                ChartSetValue {
                    key: "resources.limits.cpu".to_string(),
                    value: "250m".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.cpu".to_string(),
                    value: "250m".to_string(),
                },
                ChartSetValue {
                    key: "resources.limits.memory".to_string(),
                    value: "384Mi".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.memory".to_string(),
                    value: "384Mi".to_string(),
                },
            ],
            ..Default::default()
        },
    };

    let mut qovery_cert_manager_webhook: Option<CommonChart> = None;
    if let DnsProviderConfiguration::QoveryDns(qovery_dns_config) = &chart_config_prerequisites.dns_provider_config {
        qovery_cert_manager_webhook = Some(CommonChart {
            chart_info: ChartInfo {
                name: "qovery-cert-manager-webhook".to_string(),
                namespace: HelmChartNamespaces::CertManager,
                path: chart_path("common/charts/qovery-cert-manager-webhook"),
                values: vec![
                    ChartSetValue {
                        key: "secret.apiKey".to_string(),
                        value: qovery_dns_config.api_key.to_string(),
                    },
                    ChartSetValue {
                        key: "secret.apiUrl".to_string(),
                        value: qovery_dns_config.api_url.to_string(), // URL standard port will be omitted from string as standard (80 HTTP & 443 HTTPS)
                    },
                    ChartSetValue {
                        key: "certManager.serviceAccountName".to_string(),
                        value: "cert-manager".to_string(),
                    },
                    ChartSetValue {
                        key: "certManager.namespace".to_string(),
                        value: HelmChartNamespaces::CertManager.to_string(),
                    },
                    // resources limits
                    ChartSetValue {
                        key: "resources.limits.memory".to_string(),
                        value: "48Mi".to_string(),
                    },
                    ChartSetValue {
                        key: "resources.requests.memory".to_string(),
                        value: "48Mi".to_string(),
                    },
                ],
                ..Default::default()
            },
        });
    }

    let metrics_server = CommonChart {
        chart_info: ChartInfo {
            name: "metrics-server".to_string(),
            path: chart_path("common/charts/metrics-server"),
            values_files: vec![chart_path("chart_values/metrics-server.yaml")],
            values: vec![
                ChartSetValue {
                    key: "resources.limits.cpu".to_string(),
                    value: "250m".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.cpu".to_string(),
                    value: "250m".to_string(),
                },
                ChartSetValue {
                    key: "resources.limits.memory".to_string(),
                    value: "256Mi".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.memory".to_string(),
                    value: "256Mi".to_string(),
                },
            ],
            ..Default::default()
        },
    };

    let kube_state_metrics = CommonChart {
        chart_info: ChartInfo {
            name: "kube-state-metrics".to_string(),
            namespace: HelmChartNamespaces::Prometheus,
            last_breaking_version_requiring_restart: Some(Version::new(4, 6, 0)),
            path: chart_path("common/charts/kube-state-metrics"),
            values: vec![
                ChartSetValue {
                    key: "prometheus.monitor.enabled".to_string(),
                    value: "true".to_string(),
                },
                ChartSetValue {
                    key: "resources.limits.cpu".to_string(),
                    value: "75m".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.cpu".to_string(),
                    value: "75m".to_string(),
                },
                ChartSetValue {
                    key: "resources.limits.memory".to_string(),
                    value: "384Mi".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.memory".to_string(),
                    value: "384Mi".to_string(),
                },
            ],
            ..Default::default()
        },
    };

    let grafana_datasources = format!(
        "
datasources:
  datasources.yaml:
    apiVersion: 1
    datasources:
      - name: Prometheus
        type: prometheus
        url: \"{}:9090\"
        access: proxy
        isDefault: true
      - name: PromLoki
        type: prometheus
        url: \"http://{}.{}.svc:3100/loki\"
        access: proxy
        isDefault: false
      - name: Loki
        type: loki
        url: \"http://{}.{}.svc:3100\"
      - name: Cloudwatch
        type: cloudwatch
        jsonData:
          authType: keys
          defaultRegion: {}
        secureJsonData:
          accessKey: '{}'
          secretKey: '{}'
      ",
        prometheus_internal_url,
        &loki.chart_info.name,
        loki_namespace,
        &loki.chart_info.name,
        loki_namespace,
        chart_config_prerequisites.region.clone(),
        qovery_terraform_config.aws_iam_cloudwatch_key,
        qovery_terraform_config.aws_iam_cloudwatch_secret,
    );

    let grafana = CommonChart {
        chart_info: ChartInfo {
            name: "grafana".to_string(),
            path: chart_path("common/charts/grafana"),
            namespace: prometheus_namespace,
            values_files: vec![chart_path("chart_values/grafana.yaml")],
            yaml_files_content: vec![ChartValuesGenerated {
                filename: "grafana_generated.yaml".to_string(),
                yaml_content: grafana_datasources,
            }],
            ..Default::default()
        },
    };

    let cert_manager = CommonChart {
        chart_info: ChartInfo {
            name: "cert-manager".to_string(),
            path: chart_path("common/charts/cert-manager"),
            namespace: HelmChartNamespaces::CertManager,
            last_breaking_version_requiring_restart: Some(Version::new(1, 4, 4)),
            values: vec![
                ChartSetValue {
                    key: "installCRDs".to_string(),
                    value: "true".to_string(),
                },
                ChartSetValue {
                    key: "startupapicheck.jobAnnotations.helm\\.sh/hook".to_string(),
                    value: "post-install\\,post-upgrade".to_string(),
                },
                ChartSetValue {
                    key: "startupapicheck.rbac.annotations.helm\\.sh/hook".to_string(),
                    value: "post-install\\,post-upgrade".to_string(),
                },
                ChartSetValue {
                    key: "startupapicheck.serviceAccount.annotations.helm\\.sh/hook".to_string(),
                    value: "post-install\\,post-upgrade".to_string(),
                },
                ChartSetValue {
                    key: "replicaCount".to_string(),
                    value: "1".to_string(),
                },
                // https://cert-manager.io/docs/configuration/acme/dns01/#setting-nameservers-for-dns01-self-check
                ChartSetValue {
                    key: "extraArgs".to_string(),
                    value: "{--dns01-recursive-nameservers-only,--dns01-recursive-nameservers=1.1.1.1:53\\,8.8.8.8:53}"
                        .to_string(),
                },
                ChartSetValue {
                    key: "prometheus.servicemonitor.enabled".to_string(),
                    value: chart_config_prerequisites.ff_metrics_history_enabled.to_string(),
                },
                ChartSetValue {
                    key: "prometheus.servicemonitor.prometheusInstance".to_string(),
                    value: "qovery".to_string(),
                },
                // resources limits
                ChartSetValue {
                    key: "resources.limits.cpu".to_string(),
                    value: "200m".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.cpu".to_string(),
                    value: "100m".to_string(),
                },
                ChartSetValue {
                    key: "resources.limits.memory".to_string(),
                    value: "1Gi".to_string(),
                },
                ChartSetValue {
                    key: "resources.requests.memory".to_string(),
                    value: "1Gi".to_string(),
                },
                // Webhooks resources limits
                ChartSetValue {
                    key: "webhook.resources.limits.cpu".to_string(),
                    value: "200m".to_string(),
                },
                ChartSetValue {
                    key: "webhook.resources.requests.cpu".to_string(),
                    value: "50m".to_string(),
                },
                ChartSetValue {
                    key: "webhook.resources.limits.memory".to_string(),
                    value: "128Mi".to_string(),
                },
                ChartSetValue {
                    key: "webhook.resources.requests.memory".to_string(),
                    value: "128Mi".to_string(),
                },
                // Cainjector resources limits
                ChartSetValue {
                    key: "cainjector.resources.limits.cpu".to_string(),
                    value: "500m".to_string(),
                },
                ChartSetValue {
                    key: "cainjector.resources.requests.cpu".to_string(),
                    value: "100m".to_string(),
                },
                ChartSetValue {
                    key: "cainjector.resources.limits.memory".to_string(),
                    value: "1Gi".to_string(),
                },
                ChartSetValue {
                    key: "cainjector.resources.requests.memory".to_string(),
                    value: "1Gi".to_string(),
                },
            ],
            ..Default::default()
        },
    };

    let cert_manager_config = get_chart_for_cert_manager_config(
        &chart_config_prerequisites.dns_provider_config,
        chart_path("common/charts/cert-manager-configs"),
        chart_config_prerequisites.dns_email_report.clone(),
        chart_config_prerequisites.acme_url.clone(),
        chart_config_prerequisites.managed_dns_helm_format.clone(),
    );

    let nginx_ingress = CommonChart {
        chart_info: ChartInfo {
            name: "nginx-ingress".to_string(),
            path: chart_path("common/charts/ingress-nginx"),
            namespace: HelmChartNamespaces::NginxIngress,
            // Because of NLB, svc can take some time to start
            timeout_in_seconds: 300,
            values_files: vec![chart_path("chart_values/nginx-ingress.yaml")],
            values: vec![
                ChartSetValue {
                    key: "controller.admissionWebhooks.enabled".to_string(),
                    value: "false".to_string(),
                },
                // Controller resources limits
                ChartSetValue {
                    key: "controller.resources.limits.cpu".to_string(),
                    value: "200m".to_string(),
                },
                ChartSetValue {
                    key: "controller.resources.requests.cpu".to_string(),
                    value: "100m".to_string(),
                },
                ChartSetValue {
                    key: "controller.resources.limits.memory".to_string(),
                    value: "768Mi".to_string(),
                },
                ChartSetValue {
                    key: "controller.resources.requests.memory".to_string(),
                    value: "768Mi".to_string(),
                },
                // Default backend resources limits
                ChartSetValue {
                    key: "defaultBackend.resources.limits.cpu".to_string(),
                    value: "20m".to_string(),
                },
                ChartSetValue {
                    key: "defaultBackend.resources.requests.cpu".to_string(),
                    value: "10m".to_string(),
                },
                ChartSetValue {
                    key: "defaultBackend.resources.limits.memory".to_string(),
                    value: "32Mi".to_string(),
                },
                ChartSetValue {
                    key: "defaultBackend.resources.requests.memory".to_string(),
                    value: "32Mi".to_string(),
                },
            ],
            ..Default::default()
        },
    };

    let pleco = CommonChart {
        chart_info: ChartInfo {
            name: "pleco".to_string(),
            path: chart_path("common/charts/pleco"),
            values_files: vec![chart_path("chart_values/pleco-aws.yaml")],
            values: vec![
                ChartSetValue {
                    key: "environmentVariables.AWS_ACCESS_KEY_ID".to_string(),
                    value: chart_config_prerequisites.aws_access_key_id.clone(),
                },
                ChartSetValue {
                    key: "environmentVariables.AWS_SECRET_ACCESS_KEY".to_string(),
                    value: chart_config_prerequisites.aws_secret_access_key.clone(),
                },
                ChartSetValue {
                    key: "environmentVariables.PLECO_IDENTIFIER".to_string(),
                    value: chart_config_prerequisites.cluster_id.clone(),
                },
                ChartSetValue {
                    key: "environmentVariables.LOG_LEVEL".to_string(),
                    value: "debug".to_string(),
                },
            ],
            ..Default::default()
        },
    };

    let cluster_agent_context = ClusterAgentContext {
        api_url: &chart_config_prerequisites.infra_options.qovery_api_url,
        api_token: &chart_config_prerequisites.infra_options.agent_version_controller_token,
        organization_long_id: &chart_config_prerequisites.organization_long_id,
        cluster_id: &chart_config_prerequisites.cluster_id,
        cluster_long_id: &chart_config_prerequisites.cluster_long_id,
        cluster_jwt_token: &chart_config_prerequisites.infra_options.jwt_token,
        grpc_url: &chart_config_prerequisites.infra_options.qovery_grpc_url,
        loki_url: if chart_config_prerequisites.ff_log_history_enabled {
            Some("http://loki.logging.svc.cluster.local:3100")
        } else {
            None
        },
    };
    let cluster_agent = get_chart_for_cluster_agent(cluster_agent_context, chart_path, None)?;

    let shell_context = ShellAgentContext {
        api_url: &chart_config_prerequisites.infra_options.qovery_api_url,
        api_token: &chart_config_prerequisites.infra_options.agent_version_controller_token,
        organization_long_id: &chart_config_prerequisites.organization_long_id,
        cluster_id: &chart_config_prerequisites.cluster_id,
        cluster_long_id: &chart_config_prerequisites.cluster_long_id,
        cluster_jwt_token: &chart_config_prerequisites.infra_options.jwt_token,
        grpc_url: &chart_config_prerequisites.infra_options.qovery_grpc_url,
    };
    let shell_agent = get_chart_for_shell_agent(shell_context, chart_path, None)?;

    // TODO: Remove this when all cluster have been updated
    let qovery_agent = CommonChart {
        chart_info: ChartInfo {
            name: "qovery-agent".to_string(),
            path: chart_path("common/charts/qovery/qovery-agent"),
            namespace: HelmChartNamespaces::Qovery,
            action: HelmAction::Destroy,
            ..Default::default()
        },
    };

    let qovery_engine_version: QoveryEngine = get_qovery_app_version(
        QoveryAppName::Engine,
        &chart_config_prerequisites.infra_options.engine_version_controller_token,
        &chart_config_prerequisites.infra_options.qovery_api_url,
        &chart_config_prerequisites.cluster_id,
    )?;

    let qovery_engine = CommonChart {
        chart_info: ChartInfo {
            name: "qovery-engine".to_string(),
            action: get_engine_helm_action_from_location(&chart_config_prerequisites.qovery_engine_location),
            path: chart_path("common/charts/qovery-engine"),
            namespace: HelmChartNamespaces::Qovery,
            timeout_in_seconds: 900,
            values: vec![
                ChartSetValue {
                    key: "image.tag".to_string(),
                    value: qovery_engine_version.version,
                },
                ChartSetValue {
                    key: "autoscaler.min_replicas".to_string(),
                    value: "1".to_string(),
                },
                ChartSetValue {
                    key: "metrics.enabled".to_string(),
                    value: chart_config_prerequisites.ff_metrics_history_enabled.to_string(),
                },
                ChartSetValue {
                    key: "volumes.storageClassName".to_string(),
                    value: "aws-ebs-gp2-0".to_string(),
                },
                ChartSetValue {
                    key: "environmentVariables.QOVERY_NATS_URL".to_string(),
                    value: chart_config_prerequisites.infra_options.qovery_nats_url.to_string(),
                },
                ChartSetValue {
                    key: "environmentVariables.QOVERY_NATS_USER".to_string(),
                    value: chart_config_prerequisites.infra_options.qovery_nats_user.to_string(),
                },
                ChartSetValue {
                    key: "environmentVariables.QOVERY_NATS_PASSWORD".to_string(),
                    value: chart_config_prerequisites
                        .infra_options
                        .qovery_nats_password
                        .to_string(),
                },
                ChartSetValue {
                    key: "environmentVariables.ORGANIZATION".to_string(),
                    value: chart_config_prerequisites.organization_id.clone(),
                },
                ChartSetValue {
                    key: "environmentVariables.CLOUD_PROVIDER".to_string(),
                    value: chart_config_prerequisites.cloud_provider.clone(),
                },
                ChartSetValue {
                    key: "environmentVariables.REGION".to_string(),
                    value: chart_config_prerequisites.region.clone(),
                },
                ChartSetValue {
                    key: "environmentVariables.LIB_ROOT_DIR".to_string(),
                    value: "/home/qovery/lib".to_string(),
                },
                ChartSetValue {
                    key: "environmentVariables.DOCKER_HOST".to_string(),
                    value: "tcp://0.0.0.0:2375".to_string(),
                },
                // engine resources limits
                ChartSetValue {
                    key: "engineResources.limits.cpu".to_string(),
                    value: "1".to_string(),
                },
                ChartSetValue {
                    key: "engineResources.requests.cpu".to_string(),
                    value: "500m".to_string(),
                },
                ChartSetValue {
                    key: "engineResources.limits.memory".to_string(),
                    value: "512Mi".to_string(),
                },
                ChartSetValue {
                    key: "engineResources.requests.memory".to_string(),
                    value: "512Mi".to_string(),
                },
                // build resources limits
                ChartSetValue {
                    key: "buildResources.limits.cpu".to_string(),
                    value: "1".to_string(),
                },
                ChartSetValue {
                    key: "buildResources.requests.cpu".to_string(),
                    value: "500m".to_string(),
                },
                ChartSetValue {
                    key: "buildResources.limits.memory".to_string(),
                    value: "4Gi".to_string(),
                },
                ChartSetValue {
                    key: "buildResources.requests.memory".to_string(),
                    value: "4Gi".to_string(),
                },
            ],
            ..Default::default()
        },
    };

    // chart deployment order matters!!!
    let mut level_1: Vec<Box<dyn HelmChart>> = vec![
        Box::new(aws_iam_eks_user_mapper),
        Box::new(q_storage_class),
        Box::new(coredns_config),
        Box::new(aws_vpc_cni_chart),
        Box::new(aws_ui_view),
    ];

    let mut level_2: Vec<Box<dyn HelmChart>> = vec![];

    let level_3: Vec<Box<dyn HelmChart>> = vec![Box::new(cert_manager)];

    let mut level_4: Vec<Box<dyn HelmChart>> = vec![Box::new(cluster_autoscaler)];

    let level_5: Vec<Box<dyn HelmChart>> = vec![
        Box::new(metrics_server),
        Box::new(aws_node_term_handler),
        Box::new(external_dns),
    ];

    let mut level_6: Vec<Box<dyn HelmChart>> = vec![Box::new(nginx_ingress)];

    let level_7: Vec<Box<dyn HelmChart>> = vec![
        Box::new(cert_manager_config),
        Box::new(qovery_agent), // TODO: Migrate to the new cluster agent
        Box::new(cluster_agent),
        Box::new(shell_agent),
        Box::new(qovery_engine),
    ];

    // observability
    if chart_config_prerequisites.ff_metrics_history_enabled {
        level_1.push(Box::new(kube_prometheus_stack));
        level_2.push(Box::new(prometheus_adapter));
        level_2.push(Box::new(kube_state_metrics));
    }
    if chart_config_prerequisites.ff_log_history_enabled {
        level_1.push(Box::new(promtail));
        level_2.push(Box::new(loki));
    }

    if chart_config_prerequisites.ff_metrics_history_enabled || chart_config_prerequisites.ff_log_history_enabled {
        level_2.push(Box::new(grafana))
    };

    if let Some(qovery_webhook) = qovery_cert_manager_webhook {
        level_4.push(Box::new(qovery_webhook));
    }

    // pleco
    if !chart_config_prerequisites.disable_pleco {
        level_6.push(Box::new(pleco));
    }

    info!("charts configuration preparation finished");
    Ok(vec![level_1, level_2, level_3, level_4, level_5, level_6, level_7])
}

// AWS CNI

#[derive(Default)]
pub struct AwsVpcCniChart {
    pub chart_info: ChartInfo,
}

impl HelmChart for AwsVpcCniChart {
    fn get_chart_info(&self) -> &ChartInfo {
        &self.chart_info
    }

    fn pre_exec(
        &self,
        kubernetes_config: &Path,
        envs: &[(String, String)],
        _payload: Option<ChartPayload>,
    ) -> Result<Option<ChartPayload>, CommandError> {
        let kinds = vec!["daemonSet", "clusterRole", "clusterRoleBinding", "serviceAccount"];
        let mut environment_variables: Vec<(&str, &str)> = envs.iter().map(|x| (x.0.as_str(), x.1.as_str())).collect();
        environment_variables.push(("KUBECONFIG", kubernetes_config.to_str().unwrap()));

        let chart_infos = self.get_chart_info();

        // Cleaning any existing crash looping pod for this helm chart
        if let Some(selector) = self.get_selector() {
            kubectl_delete_crash_looping_pods(
                &kubernetes_config,
                Some(chart_infos.get_namespace_string().as_str()),
                Some(selector.as_str()),
                environment_variables.clone(),
            )?;
        }

        match self.enable_cni_managed_by_helm(kubernetes_config, envs) {
            true => {
                for kind in kinds {
                    // Setting annotations and labels on kind/aws-node
                    let steps = || -> Result<(), CommandError> {
                        let label = format!("meta.helm.sh/release-name={}", self.chart_info.name);
                        let args = vec![
                            "-n",
                            "kube-system",
                            "annotate",
                            "--overwrite",
                            kind,
                            "aws-node",
                            label.as_str(),
                        ];
                        let mut stdout = "".to_string();
                        let mut stderr = "".to_string();

                        kubectl_exec_with_output(
                            args.clone(),
                            environment_variables.clone(),
                            &mut |out| stdout = format!("{}\n{}", stdout, out),
                            &mut |out| stderr = format!("{}\n{}", stderr, out),
                        )?;

                        let args = vec![
                            "-n",
                            "kube-system",
                            "annotate",
                            "--overwrite",
                            kind,
                            "aws-node",
                            "meta.helm.sh/release-namespace=kube-system",
                        ];
                        let mut stdout = "".to_string();
                        let mut stderr = "".to_string();

                        kubectl_exec_with_output(
                            args.clone(),
                            environment_variables.clone(),
                            &mut |out| stdout = format!("{}\n{}", stdout, out),
                            &mut |out| stderr = format!("{}\n{}", stderr, out),
                        )?;

                        let args = vec![
                            "-n",
                            "kube-system",
                            "label",
                            "--overwrite",
                            kind,
                            "aws-node",
                            "app.kubernetes.io/managed-by=Helm",
                        ];
                        let mut stdout = "".to_string();
                        let mut stderr = "".to_string();

                        kubectl_exec_with_output(
                            args.clone(),
                            environment_variables.clone(),
                            &mut |out| stdout = format!("{}\n{}", stdout, out),
                            &mut |out| stderr = format!("{}\n{}", stderr, out),
                        )?;

                        Ok(())
                    };

                    steps()?;
                }

                // sleep in order to be sure the daemonset is updated
                sleep(Duration::from_secs(30))
            }
            false => {} // AWS CNI is already supported by Helm, nothing to do
        };

        Ok(None)
    }
}

impl AwsVpcCniChart {
    /// this is required to know if we need to keep old annotation/labels values or not
    fn is_cni_old_installed_version(
        &self,
        kubernetes_config: &Path,
        envs: &[(String, String)],
    ) -> Result<bool, CommandError> {
        let environment_variables = envs.iter().map(|x| (x.0.as_str(), x.1.as_str())).collect();

        match kubectl_exec_get_daemonset(
            kubernetes_config,
            "aws-node",
            self.namespace().as_str(),
            None,
            environment_variables,
        ) {
            Ok(x) => match x.spec {
                None => Err(CommandError::new_from_safe_message(format!(
                    "Spec was not found in json output while looking at daemonset {}",
                    &self.chart_info.name
                ))),
                Some(spec) => match spec.selector.match_labels.k8s_app {
                    Some(x) if x == "aws-node" => Ok(true),
                    _ => Ok(false),
                },
            },
            Err(e) => Err(CommandError::new(
                format!(
                    "Error while getting daemonset info for chart {}, won't deploy CNI chart. {}",
                    &self.chart_info.name,
                    e.message(ErrorMessageVerbosity::SafeOnly)
                ),
                e.message_raw(),
                e.env_vars(),
            )),
        }
    }

    fn enable_cni_managed_by_helm(&self, kubernetes_config: &Path, envs: &[(String, String)]) -> bool {
        let environment_variables = envs.iter().map(|x| (x.0.as_str(), x.1.as_str())).collect();

        match kubectl_exec_get_daemonset(
            kubernetes_config,
            &self.chart_info.name,
            self.namespace().as_str(),
            Some("k8s-app=aws-node,app.kubernetes.io/managed-by=Helm"),
            environment_variables,
        ) {
            Ok(x) => x.items.is_some() && x.items.unwrap().is_empty(),
            Err(e) => {
                error!(
                    "error while getting daemonset info for chart {}, won't deploy CNI chart. {:?}",
                    &self.chart_info.name, e
                );
                false
            }
        }
    }
}
