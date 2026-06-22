targetScope = 'resourceGroup'

@description('Azure region for all resources.')
param location string = resourceGroup().location

@description('Prefix for resource names.')
@minLength(3)
param namePrefix string = 'domyn-nexus'

@description('Domyn Nexus container image to run on the VM.')
param nexusImage string

@description('Admin SSH public key for the VM.')
param adminSshPublicKey string

@secure()
@description('Admin API token stored in Key Vault. The VM reads it at boot through managed identity.')
param adminToken string

@secure()
@description('Read/write API token stored in Key Vault. The VM reads it at boot through managed identity.')
param writerToken string

@secure()
@description('Read-only API token stored in Key Vault. The VM reads it at boot through managed identity.')
param readerToken string

@secure()
@description('Optional PEM-encoded TLS certificate stored in Key Vault. Leave empty to generate a demo self-signed certificate on the VM.')
param tlsCertPem string = ''

@secure()
@description('Optional PEM-encoded TLS private key stored in Key Vault. Leave empty to generate a demo self-signed certificate on the VM.')
param tlsKeyPem string = ''

@description('Allowed CIDR for inbound HTTPS and Bolt. Narrow this before shared use.')
param allowedSourceCidr string = '*'

@description('Linux admin username for the VM.')
param adminUsername string = 'azureuser'

@description('VM size.')
param vmSize string = 'Standard_D4s_v5'

@description('Managed data disk size in GB.')
param dataDiskSizeGb int = 128

@description('Globally unique storage account name for backup artifacts.')
@minLength(3)
@maxLength(24)
param backupStorageAccountName string = 'domynnx${uniqueString(resourceGroup().id)}'

var vmName = '${namePrefix}-vm'
var vnetName = '${namePrefix}-vnet'
var subnetName = 'nexus'
var nsgName = '${namePrefix}-nsg'
var publicIpName = '${namePrefix}-pip'
var nicName = '${namePrefix}-nic'
var diskName = '${namePrefix}-data'
var identityName = '${namePrefix}-identity'
var keyVaultName = take(toLower(replace('${namePrefix}${uniqueString(resourceGroup().id)}kv', '-', '')), 24)
var backupContainerName = 'nexus-backups'
var logAnalyticsName = '${namePrefix}-logs'
var dataCollectionRuleName = '${namePrefix}-dcr'
var keyVaultSecretsUserRoleDefinitionId = subscriptionResourceId('Microsoft.Authorization/roleDefinitions', '4633458b-17de-408a-b874-0445c86b69e6c')
var storageBlobDataContributorRoleDefinitionId = subscriptionResourceId('Microsoft.Authorization/roleDefinitions', 'ba92f5b4-2d11-453d-a403-e96b0029c9fe')

resource vnet 'Microsoft.Network/virtualNetworks@2023-09-01' = {
  name: vnetName
  location: location
  properties: {
    addressSpace: {
      addressPrefixes: [
        '10.44.0.0/16'
      ]
    }
    subnets: [
      {
        name: subnetName
        properties: {
          addressPrefix: '10.44.1.0/24'
          networkSecurityGroup: {
            id: nsg.id
          }
        }
      }
    ]
  }
}

resource nsg 'Microsoft.Network/networkSecurityGroups@2023-09-01' = {
  name: nsgName
  location: location
  properties: {
    securityRules: [
      {
        name: 'allow-https'
        properties: {
          priority: 100
          access: 'Allow'
          direction: 'Inbound'
          protocol: 'Tcp'
          sourcePortRange: '*'
          destinationPortRange: '8443'
          sourceAddressPrefix: allowedSourceCidr
          destinationAddressPrefix: '*'
        }
      }
      {
        name: 'allow-bolt'
        properties: {
          priority: 110
          access: 'Allow'
          direction: 'Inbound'
          protocol: 'Tcp'
          sourcePortRange: '*'
          destinationPortRange: '7687'
          sourceAddressPrefix: allowedSourceCidr
          destinationAddressPrefix: '*'
        }
      }
      {
        name: 'allow-ssh'
        properties: {
          priority: 120
          access: 'Allow'
          direction: 'Inbound'
          protocol: 'Tcp'
          sourcePortRange: '*'
          destinationPortRange: '22'
          sourceAddressPrefix: allowedSourceCidr
          destinationAddressPrefix: '*'
        }
      }
    ]
  }
}

resource publicIp 'Microsoft.Network/publicIPAddresses@2023-09-01' = {
  name: publicIpName
  location: location
  sku: {
    name: 'Standard'
  }
  properties: {
    publicIPAllocationMethod: 'Static'
  }
}

resource nic 'Microsoft.Network/networkInterfaces@2023-09-01' = {
  name: nicName
  location: location
  properties: {
    ipConfigurations: [
      {
        name: 'ipconfig1'
        properties: {
          privateIPAllocationMethod: 'Dynamic'
          subnet: {
            id: vnet.properties.subnets[0].id
          }
          publicIPAddress: {
            id: publicIp.id
          }
        }
      }
    ]
  }
}

resource dataDisk 'Microsoft.Compute/disks@2023-10-02' = {
  name: diskName
  location: location
  sku: {
    name: 'Premium_LRS'
  }
  properties: {
    creationData: {
      createOption: 'Empty'
    }
    diskSizeGB: dataDiskSizeGb
  }
}

resource keyVault 'Microsoft.KeyVault/vaults@2023-07-01' = {
  name: keyVaultName
  location: location
  properties: {
    tenantId: subscription().tenantId
    sku: {
      family: 'A'
      name: 'standard'
    }
    enableRbacAuthorization: true
    enabledForDeployment: true
  }
}

resource managedIdentity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: identityName
  location: location
}

resource adminTokenSecret 'Microsoft.KeyVault/vaults/secrets@2023-07-01' = {
  parent: keyVault
  name: 'nexus-admin-token'
  properties: {
    value: adminToken
  }
}

resource tlsCertSecret 'Microsoft.KeyVault/vaults/secrets@2023-07-01' = if (!empty(tlsCertPem)) {
  parent: keyVault
  name: 'nexus-tls-cert-pem'
  properties: {
    value: tlsCertPem
  }
}

resource tlsKeySecret 'Microsoft.KeyVault/vaults/secrets@2023-07-01' = if (!empty(tlsKeyPem)) {
  parent: keyVault
  name: 'nexus-tls-key-pem'
  properties: {
    value: tlsKeyPem
  }
}

resource writerTokenSecret 'Microsoft.KeyVault/vaults/secrets@2023-07-01' = {
  parent: keyVault
  name: 'nexus-writer-token'
  properties: {
    value: writerToken
  }
}

resource readerTokenSecret 'Microsoft.KeyVault/vaults/secrets@2023-07-01' = {
  parent: keyVault
  name: 'nexus-reader-token'
  properties: {
    value: readerToken
  }
}

resource backupStorage 'Microsoft.Storage/storageAccounts@2023-01-01' = {
  name: backupStorageAccountName
  location: location
  sku: {
    name: 'Standard_LRS'
  }
  kind: 'StorageV2'
  properties: {
    accessTier: 'Hot'
    allowBlobPublicAccess: false
    minimumTlsVersion: 'TLS1_2'
  }
}

resource backupContainer 'Microsoft.Storage/storageAccounts/blobServices/containers@2023-01-01' = {
  name: '${backupStorage.name}/default/${backupContainerName}'
  properties: {
    publicAccess: 'None'
  }
}

resource logAnalytics 'Microsoft.OperationalInsights/workspaces@2022-10-01' = {
  name: logAnalyticsName
  location: location
  properties: {
    retentionInDays: 30
    sku: {
      name: 'PerGB2018'
    }
  }
}

resource dataCollectionRule 'Microsoft.Insights/dataCollectionRules@2022-06-01' = {
  name: dataCollectionRuleName
  location: location
  kind: 'Linux'
  properties: {
    dataSources: {
      performanceCounters: [
        {
          name: 'domyn-nexus-vm-performance'
          streams: [
            'Microsoft-Perf'
          ]
          samplingFrequencyInSeconds: 60
          counterSpecifiers: [
            'Processor(*)\\% Processor Time'
            'Memory(*)\\Available MBytes'
            'Logical Disk(*)\\% Free Space'
            'Logical Disk(*)\\Disk Reads/sec'
            'Logical Disk(*)\\Disk Writes/sec'
            'Network(*)\\Bytes Total/sec'
          ]
        }
      ]
      syslog: [
        {
          name: 'domyn-nexus-syslog'
          streams: [
            'Microsoft-Syslog'
          ]
          facilityNames: [
            'auth'
            'authpriv'
            'daemon'
            'syslog'
            'user'
          ]
          logLevels: [
            'Info'
            'Notice'
            'Warning'
            'Error'
            'Critical'
            'Alert'
            'Emergency'
          ]
        }
      ]
    }
    destinations: {
      logAnalytics: [
        {
          name: 'centralWorkspace'
          workspaceResourceId: logAnalytics.id
        }
      ]
    }
    dataFlows: [
      {
        streams: [
          'Microsoft-Perf'
        ]
        destinations: [
          'centralWorkspace'
        ]
      }
      {
        streams: [
          'Microsoft-Syslog'
        ]
        destinations: [
          'centralWorkspace'
        ]
      }
    ]
  }
}

var cloudInit = loadTextContent('cloud-init.yaml')
var cloudInitWithImage = replace(cloudInit, '__NEXUS_IMAGE__', nexusImage)
var cloudInitWithIdentity = replace(cloudInitWithImage, '__MANAGED_IDENTITY_CLIENT_ID__', managedIdentity.properties.clientId)
var cloudInitWithVault = replace(cloudInitWithIdentity, '__KEY_VAULT_NAME__', keyVault.name)
var cloudInitWithStorage = replace(cloudInitWithVault, '__BACKUP_STORAGE_ACCOUNT__', backupStorage.name)
var renderedCloudInit = replace(cloudInitWithStorage, '__BACKUP_CONTAINER__', backupContainerName)

resource vmKeyVaultSecretsUser 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(keyVault.id, managedIdentity.id, 'domyn-nexus-keyvault-secrets-user')
  scope: keyVault
  properties: {
    roleDefinitionId: keyVaultSecretsUserRoleDefinitionId
    principalId: managedIdentity.properties.principalId
    principalType: 'ServicePrincipal'
  }
  dependsOn: [
    adminTokenSecret
    writerTokenSecret
    readerTokenSecret
  ]
}

resource vmBlobDataContributor 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(backupStorage.id, managedIdentity.id, 'domyn-nexus-blob-data-contributor')
  scope: backupStorage
  properties: {
    roleDefinitionId: storageBlobDataContributorRoleDefinitionId
    principalId: managedIdentity.properties.principalId
    principalType: 'ServicePrincipal'
  }
  dependsOn: [
    backupContainer
  ]
}

resource vm 'Microsoft.Compute/virtualMachines@2023-09-01' = {
  name: vmName
  location: location
  identity: {
    type: 'UserAssigned'
    userAssignedIdentities: {
      '${managedIdentity.id}': {}
    }
  }
  properties: {
    hardwareProfile: {
      vmSize: vmSize
    }
    osProfile: {
      computerName: vmName
      adminUsername: adminUsername
      customData: base64(renderedCloudInit)
      linuxConfiguration: {
        disablePasswordAuthentication: true
        ssh: {
          publicKeys: [
            {
              path: '/home/${adminUsername}/.ssh/authorized_keys'
              keyData: adminSshPublicKey
            }
          ]
        }
      }
    }
    storageProfile: {
      imageReference: {
        publisher: 'Canonical'
        offer: '0001-com-ubuntu-server-jammy'
        sku: '22_04-lts-gen2'
        version: 'latest'
      }
      osDisk: {
        createOption: 'FromImage'
        managedDisk: {
          storageAccountType: 'Premium_LRS'
        }
      }
      dataDisks: [
        {
          lun: 0
          createOption: 'Attach'
          managedDisk: {
            id: dataDisk.id
          }
        }
      ]
    }
    networkProfile: {
      networkInterfaces: [
        {
          id: nic.id
        }
      ]
    }
  }
  dependsOn: [
    vmKeyVaultSecretsUser
    vmBlobDataContributor
    tlsCertSecret
    tlsKeySecret
  ]
}

resource azureMonitorAgent 'Microsoft.Compute/virtualMachines/extensions@2023-09-01' = {
  parent: vm
  name: 'AzureMonitorLinuxAgent'
  location: location
  properties: {
    publisher: 'Microsoft.Azure.Monitor'
    type: 'AzureMonitorLinuxAgent'
    typeHandlerVersion: '1.0'
    autoUpgradeMinorVersion: true
    enableAutomaticUpgrade: true
  }
}

resource vmDataCollectionRuleAssociation 'Microsoft.Insights/dataCollectionRuleAssociations@2022-06-01' = {
  name: 'domyn-nexus-dcr-association'
  scope: vm
  properties: {
    dataCollectionRuleId: dataCollectionRule.id
  }
  dependsOn: [
    azureMonitorAgent
  ]
}

output publicIp string = publicIp.properties.ipAddress
output httpsUrl string = 'https://${publicIp.properties.ipAddress}:8443'
output boltEndpoint string = '${publicIp.properties.ipAddress}:7687'
output keyVaultName string = keyVault.name
output backupStorageAccountName string = backupStorage.name
output backupContainerName string = backupContainerName
output logAnalyticsWorkspaceName string = logAnalytics.name
output dataCollectionRuleName string = dataCollectionRule.name
