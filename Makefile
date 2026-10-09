REGISTRY   ?=
NAME       ?= lark-bot-agent
PLATFORM   ?= linux/amd64
LATEST     ?= 0
LOAD       ?= 0

ifneq ($(strip $(REGISTRY)),)
IMAGE := $(patsubst %/,%,$(REGISTRY))/$(NAME)
else
IMAGE := $(NAME)
endif

GIT_SHA    := $(shell git rev-parse --short=12 HEAD 2>/dev/null || echo nocommit)
BUILD_TIME := $(shell date +%Y%m%d%H%M)
TAG        ?= $(GIT_SHA)-$(BUILD_TIME)

CODEX_ACP_VERSION        ?= 2.1.1
CLAUDE_AGENT_ACP_VERSION ?= 0.88.0

BUILD_ARGS := --build-arg CODEX_ACP_VERSION=$(CODEX_ACP_VERSION) \
              --build-arg CLAUDE_AGENT_ACP_VERSION=$(CLAUDE_AGENT_ACP_VERSION)

TAGS := -t $(IMAGE):$(TAG)
ifeq ($(LATEST),1)
TAGS += -t $(IMAGE):latest
endif

ifeq ($(LOAD),1)
OUTPUT := --load
else
OUTPUT := --push
endif

.PHONY: image tag test clean

image:
	docker buildx build --platform $(PLATFORM) $(BUILD_ARGS) $(TAGS) $(OUTPUT) .
	@echo "$(IMAGE):$(TAG)"

tag:
	@echo $(IMAGE):$(TAG)

test:
	cargo test

clean:
	cargo clean
