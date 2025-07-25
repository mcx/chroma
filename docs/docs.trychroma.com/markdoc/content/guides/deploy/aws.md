# AWS Deployment

{% Banner type="tip" %}

**Chroma Cloud**

Chroma Cloud, our fully managed hosted service is here. [Sign up for free](https://trychroma.com/signup).

{% /Banner %}

## A Simple AWS Deployment

You can deploy Chroma on a long-running server, and connect to it
remotely.

There are many possible configurations, but for convenience we have
provided a very simple AWS CloudFormation template to experiment with
deploying Chroma to EC2 on AWS.

{% Banner type="warn" %}

Chroma and its underlying database [need at least 2GB of RAM](./performance#results-summary),
which means it won't fit on the 1gb instances provided as part of the
AWS Free Tier. This template uses a [`t3.small`](https://aws.amazon.com/ec2/instance-types/t3/#Product%20Details) EC2 instance, which
costs about two cents an hour, or $15 for a full month, and gives you 2GiB of memory. If you follow these
instructions, AWS will bill you accordingly.

{% /Banner %}

{% Banner type="warn"  %}

By default, this template saves all data on a single
volume. When you delete or replace it, the data will disappear. For
serious production use (with high availability, backups, etc.) please
read and understand the CloudFormation template and use it as a basis
for what you need, or reach out to the Chroma team for assistance.

{% /Banner %}

### Step 1: Get an AWS Account

You will need an AWS Account. You can use one you already have, or
[create a new one](https://aws.amazon.com).

### Step 2: Get credentials

For this example, we will be using the AWS command line
interface. There are
[several ways](https://docs.aws.amazon.com/cli/latest/userguide/getting-started-prereqs.html)
to configure the AWS CLI, but for the purposes of these examples we
will presume that you have
[obtained an AWS access key](https://docs.aws.amazon.com/IAM/latest/UserGuide/id_credentials_access-keys.html)
and will be using environment variables to configure AWS.

Export the `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` environment variables in your shell:

```terminal
export AWS_ACCESS_KEY_ID=**\*\***\*\*\*\***\*\***
export AWS_SECRET_ACCESS_KEY=****\*\*****\*\*****\*\*****
```

You can also configure AWS to use a region of your choice using the
`AWS_REGION` environment variable:

```terminal
export AWS_REGION=us-east-1
```

### Step 3: Run CloudFormation

Chroma publishes a [CloudFormation template](https://s3.amazonaws.com/public.trychroma.com/cloudformation/latest/chroma.cf.json) to S3 for each release.

To launch the template using AWS CloudFormation, run the following command line invocation.

Replace `--stack-name my-chroma-stack` with a different stack name, if you wish.

```terminal
aws cloudformation create-stack --stack-name my-chroma-stack --template-url https://s3.amazonaws.com/public.trychroma.com/cloudformation/latest/chroma.cf.json
```

Wait a few minutes for the server to boot up, and Chroma will be
available! You can get the public IP address of your new Chroma server using the AWS console, or using the following command:

```terminal
aws cloudformation describe-stacks --stack-name my-chroma-stack --query 'Stacks[0].Outputs'
```

Note that even after the IP address of your instance is available, it may still take a few minutes for Chroma to be up and running.

#### Customize the Stack (optional)

The CloudFormation template allows you to pass particular key/value
pairs to override aspects of the stack. Available keys are:

- `InstanceType` - the AWS instance type to run (default: `t3.small`)
- `KeyName` - the AWS EC2 KeyPair to use, allowing to access the instance via SSH (default: none)

To set a CloudFormation stack's parameters using the AWS CLI, use the
`--parameters` command line option. Parameters must be specified using
the format `ParameterName={parameter},ParameterValue={value}`.

For example, the following command launches a new stack similar to the
above, but on a `m5.4xlarge` EC2 instance, and adding a KeyPair named
`mykey` so anyone with the associated private key can SSH into the
machine:

```terminal
aws cloudformation create-stack --stack-name my-chroma-stack --template-url https://s3.amazonaws.com/public.trychroma.com/cloudformation/latest/chroma.cf.json \
 --parameters ParameterKey=KeyName,ParameterValue=mykey \
 ParameterKey=InstanceType,ParameterValue=m5.4xlarge
```

### Step 4: Chroma Client Set-Up

{% Tabs %}

{% Tab label="python" %}
Once your EC2 instance is up and running with Chroma, all
you need to do is configure your `HttpClient` to use the server's IP address and port
`8000`. Since you are running a Chroma server on AWS, our [thin-client package](./python-thin-client) may be enough for your application.

```python
import chromadb

chroma_client = chromadb.HttpClient(
    host="<Your Chroma instance IP>",
    port=8000
)
chroma_client.heartbeat()
```
{% /Tab %}

{% Tab label="typescript" %}
Once your EC2 instance is up and running with Chroma, all
you need to do is configure your `ChromaClient` to use the server's IP address and port
`8000`.

```typescript
import { ChromaClient } from "chromadb";

const chromaClient = new ChromaClient({
    host: "<Your Chroma instance IP>",
    port: 8000
})
chromaClient.heartbeat()
```
{% /Tab %}

{% /Tabs %}

### Step 5: Clean Up (optional).

To destroy the stack and remove all AWS resources, use the AWS CLI `delete-stack` command.

{% note type="warning" title="Note" %}
This will destroy all the data in your Chroma database,
unless you've taken a snapshot or otherwise backed it up.
{% /note %}

```terminal
aws cloudformation delete-stack --stack-name my-chroma-stack
```

## Observability with AWS

Chroma is instrumented with [OpenTelemetry](https://opentelemetry.io/) hooks for observability. We currently only exports OpenTelemetry [traces](https://opentelemetry.io/docs/concepts/signals/traces/). These should allow you to understand how requests flow through the system and quickly identify bottlenecks. Check out the [observability docs](../administration/observability) for a full explanation of the available parameters.

To enable tracing on your Chroma server, simply pass your desired values as arguments when creating your Cloudformation stack:

```terminal
aws cloudformation create-stack --stack-name my-chroma-stack --template-url https://s3.amazonaws.com/public.trychroma.com/cloudformation/latest/chroma.cf.json \
 --parameters ParameterKey=ChromaOtelCollectionEndpoint,ParameterValue="api.honeycomb.com" \
 ParameterKey=ChromaOtelServiceName,ParameterValue="chromadb" \
 ParameterKey=ChromaOtelCollectionHeaders,ParameterValue="{'x-honeycomb-team': 'abc'}"
```

## Troubleshooting

#### Error: No default VPC for this user

If you get an error saying `No default VPC for this user` when creating `ChromaInstanceSecurityGroup`, head to [AWS VPC section](https://us-east-1.console.aws.amazon.com/vpc/home?region=us-east-1#vpcs) and create a default VPC for your user.
